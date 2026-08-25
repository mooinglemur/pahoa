//! The room actor.
//!
//! One task owns all mutable room state, so nothing is locked and nothing can
//! observe a half-applied change. The rule that makes this fast rather than a
//! bottleneck:
//!
//! > The actor awaits exactly one thing — its mailbox. Every outbound send is
//! > `try_send`. No parsing, no compression, no I/O, and no `.await` on a
//! > channel or socket happens here.
//!
//! Inbound frames are parsed in the per-connection reader task; outbound frames
//! are encoded here once and then handed to shards as [`Bytes`], whose clone is
//! a refcount bump. Everything genuinely expensive lives off this task.

use crate::save::SaveSink;
use crate::shard::{Outbound, ShardMsg, Shards};
use crate::ws::Outgoing;
use pahoa_proto::{ClientPacket, ServerPacket, encode};
use pahoa_room::{CloseReason, ConnId, EffectSink, Recipients, Room};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub enum ActorMsg {
    Connected {
        conn: ConnId,
        tx: mpsc::Sender<Outbound>,
        /// Closes this connection when `tx` is too full to carry the close.
        /// See [`crate::shard::CloseSignal`].
        close: crate::shard::CloseSignal,
        /// The deflate window this connection negotiated, if any. Decides which
        /// variant of a broadcast its shard hands it.
        deflate: Option<u8>,
        /// This connection's share of the outbound byte budget.
        budget: crate::budget::ConnHandle,
        /// Whether this connection arrived on the scoped port.
        ///
        /// Decided by the listener and fixed for the connection's life, because
        /// a policy the client could change would be wiped by the next
        /// `ConnectUpdate` — see `docs/scoped-feed.md`.
        feed: pahoa_room::FeedPolicy,
    },
    Packets {
        conn: ConnId,
        packets: Vec<ClientPacket>,
        /// Wire bytes of the message these came in, carried because only the
        /// actor knows which slot to charge them to — the reader task that
        /// measured them has a `ConnId` and nothing else.
        bytes: usize,
    },
    /// The reader could not decode a frame. Reproduces the reference server's
    /// behavior of dropping the socket rather than answering `InvalidPacket`.
    DecodeFailed {
        conn: ConnId,
        detail: String,
    },
    Disconnected {
        conn: ConnId,
    },
    /// The live figures the HTTP surface reports.
    ///
    /// Answered from inside the loop, where `&mut Room` already is, and replied
    /// to through a `oneshot` — whose `send` never blocks, so the actor's
    /// "awaits exactly one thing, its mailbox" invariant survives even a caller
    /// that has already gone away.
    Live {
        reply: tokio::sync::oneshot::Sender<crate::http::Live>,
    },
    /// Everything `/admin/v1/status` reports that only the actor can see.
    ///
    /// Separate from [`ActorMsg::Live`] because this walks every slot, and the
    /// public route is reached by a readiness probe on a schedule.
    Status {
        reply: tokio::sync::oneshot::Sender<crate::http::Status>,
    },
    /// A snapshot for the tracker API.
    ///
    /// Answers with `Arc` clones rather than a rendered document: the JSON runs
    /// to megabytes on a large room, and serializing it on the actor would put
    /// that on the one task that owns room state.
    Tracker {
        reply: tokio::sync::oneshot::Sender<pahoa_room::tracker::TrackerData>,
    },
    /// An administrative command, with its target already resolved.
    Admin {
        command: pahoa_room::AdminCommand,
        reply: tokio::sync::oneshot::Sender<pahoa_room::AdminOutcome>,
    },
    /// Rotate one slot's password on a live room.
    ///
    /// Not persisted, because nothing about a password is: the environment is
    /// authoritative on every start, which is exactly what lets a rotation
    /// survive a restart rather than reverting to what was on disk.
    SetSlotPassword {
        slot: u32,
        password: Option<String>,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    /// Read or change a filter. `slot` of `None` is the room-wide default.
    Filter {
        slot: Option<u32>,
        edit: FilterEdit,
        reply: tokio::sync::oneshot::Sender<FilterReply>,
    },
    Shutdown,
}

/// What a filter request wants done.
///
/// Mirrors the HTTP methods rather than being one "set" with flags, because the
/// four differ in what a caller has to know: `Replace` needs the whole intended
/// state, `Merge` needs only what changed, `Remove` needs only matchers, and
/// `Read` needs nothing.
#[derive(Debug)]
pub enum FilterEdit {
    Read,
    Replace(pahoa_room::filter::Filter),
    Merge(pahoa_room::filter::Filter),
    /// Rules identified by matcher; their probabilities are ignored.
    Remove(pahoa_room::filter::Filter),
    Clear,
}

#[derive(Debug)]
pub enum FilterReply {
    Ok {
        /// **This resource's own rules** — what `PUT`, `PATCH` and `DELETE`
        /// operate on.
        ///
        /// `null` when there is no ruleset here at all, `[]` when there is one
        /// and it is empty. Those are different states — for a slot, the first
        /// inherits the room's filter and the second is an exemption from it —
        /// and the field that a caller edits is the one that should say so.
        /// [`FilterReply::Ok::inherited`] is a convenience derived from this,
        /// not the other way round.
        ///
        /// Deliberately not the effective filter. If a `GET` returned the
        /// inherited rules, a `PATCH` would either merge into them — silently
        /// forking the room's filter down onto the slot, so later room changes
        /// stopped reaching it — or ignore what it had just shown. Both are
        /// surprising; showing what is actually being edited is not.
        rules: serde_json::Value,
        /// What actually applies to this slot, which is the room's when the
        /// slot has no filter of its own.
        effective: serde_json::Value,
        /// Whether `effective` came from the room rather than from here.
        ///
        /// Derived — it is exactly `rules == null` on a slot — and kept because
        /// it saves every caller encoding that rule for themselves.
        inherited: bool,
        /// How many rules a `DELETE` with a body took.
        removed: usize,
    },
    /// The slot is not in this seed.
    UnknownSlot,
    /// The edit was refused, with the reason.
    Refused(String),
}

/// Bridges the room's effects onto the shards.
///
/// Encodes once per effect and never blocks, so a slow client cannot stall the
/// room.
struct Dispatcher<'a> {
    shards: &'a Shards,
    dirty: bool,
    /// Membership changes the shards need, collected during a handler and
    /// flushed after, so the room's borrow is released first.
    updates: Vec<(ConnId, crate::shard::Membership)>,
    /// Where checks go when the room is keeping a history. `None` is the
    /// common case and costs one branch per check.
    journal: Option<&'a crate::journal::Journal>,
}

impl<'a> Dispatcher<'a> {
    fn new(shards: &'a Shards, journal: Option<&'a crate::journal::Journal>) -> Self {
        Self {
            shards,
            dirty: false,
            updates: Vec::new(),
            journal,
        }
    }
}

impl EffectSink for Dispatcher<'_> {
    fn send(&mut self, to: ConnId, msgs: &[ServerPacket]) {
        if msgs.is_empty() {
            return;
        }
        // Counted here for the same reason the tag is built here: this is the
        // last point at which these are packets rather than bytes.
        for msg in msgs {
            crate::metrics::record_packet_out(msg.cmd());
        }
        // Tagged from the packets, before they become bytes — the shard sees
        // only a frame and cannot tell a chat line from an item delivery.
        let tag = pahoa_room::filter::outbound_tag(msgs).map(std::sync::Arc::new);
        let msg = Outgoing::text(encode(msgs).as_bytes());
        self.shards.tell(to, ShardMsg::Send { conn: to, msg, tag });
    }

    fn broadcast(&mut self, to: Recipients, msgs: &[ServerPacket]) {
        if msgs.is_empty() {
            return;
        }
        // Once per message, not once per recipient: this counter is what the
        // room produced, and `pahoa_frames_out_total` is what fan-out made of
        // it. One chat line to two thousand slots is one here.
        for msg in msgs {
            crate::metrics::record_packet_out(msg.cmd());
        }
        // Encoded and framed once for every recipient across every shard.
        // Compression deliberately happens further out, in the shards — see
        // `Shards::broadcast`.
        let tag = pahoa_room::filter::outbound_tag(msgs).map(std::sync::Arc::new);
        let msg = Outgoing::text(encode(msgs).as_bytes());
        self.shards.broadcast(to, msg, tag);
    }

    fn membership_changed(
        &mut self,
        conn: ConnId,
        auth: bool,
        no_text: bool,
        slot: Option<(u32, u32)>,
    ) {
        // Told immediately rather than queued into `updates`, because the
        // effect that follows is the join broadcast and the shard must already
        // know this connection counts as a recipient.
        self.shards.tell(
            conn,
            ShardMsg::Update {
                conn,
                auth,
                no_text,
                slot,
            },
        );
    }

    fn filter_changed(
        &mut self,
        conn: ConnId,
        filter: Option<std::sync::Arc<pahoa_room::filter::Filter>>,
    ) {
        self.shards.tell(conn, ShardMsg::SetFilter { conn, filter });
    }

    fn close(&mut self, conn: ConnId, reason: CloseReason) {
        let text = match reason {
            CloseReason::ProtocolError(_) => "protocol error",
            CloseReason::TooSlow => "client too slow",
            CloseReason::ServerShutdown => "server shutting down",
            CloseReason::Kicked => "disconnected by an administrator",
        };
        self.shards
            .tell(conn, ShardMsg::Close { conn, reason: text });
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn journal_check(&mut self, record: pahoa_room::CheckRecord) {
        // `record` never blocks: it drops rather than wait, so a stalled disk
        // slows the journal and not the multiworld.
        if let Some(journal) = self.journal {
            journal.record(record);
        }
    }

    fn journal_event(&mut self, event: pahoa_room::JournalEvent) {
        if let Some(journal) = self.journal {
            journal.event(event);
        }
    }
}

/// Unix time as a float, the scale `RoomInfo.time` and the room's clock use.
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Where and how often the room persists itself.
#[derive(Clone)]
pub struct SaveConfig {
    /// `None` runs the room without persistence, which is what most tests want.
    pub store: Option<Arc<dyn SaveSink>>,
    /// How long the room may lose on an unclean stop. Also the coalescing
    /// window: ticks that land while a save is running are dropped, not queued.
    pub interval: Duration,
    /// Deflate the body. Costs CPU on a background thread and saves bytes on a
    /// network filesystem, which is the trade worth making.
    pub compress: bool,
    /// How long a shutdown flush may take before the room stops waiting for it.
    ///
    /// The flush is a nicety, not the guarantee: SIGKILL past
    /// `terminationGracePeriodSeconds`, node loss, OOM kill and spot preemption
    /// all skip it. The cadence above is what actually bounds data loss.
    pub shutdown_timeout: Duration,
    /// The room's durable history, if it is keeping one.
    ///
    /// Here rather than as its own parameter because it is persistence: it
    /// lives in the save directory, it appends across restarts, and a room with
    /// no `--save-dir` has nowhere to put it.
    pub journal: Option<crate::journal::Journal>,
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            store: None,
            interval: Duration::from_secs(60),
            compress: true,
            shutdown_timeout: Duration::from_secs(10),
            journal: None,
        }
    }
}

/// A save running on a blocking thread, plus what the room knows about it.
///
/// The rule this exists to enforce: **at most one save is ever in flight, and
/// the actor never waits for it.** Without the first half, a slow filesystem
/// accumulates snapshots in memory — each pinning the `Arc`s it captured — and
/// that is an out-of-memory path that only shows up on a bad day. Without the
/// second, the room stalls for as long as the disk does.
struct Saver {
    config: SaveConfig,
    in_flight: Option<JoinHandle<std::io::Result<usize>>>,
    /// State has changed since the last save was *started*.
    dirty: bool,
}

impl Saver {
    fn new(config: SaveConfig) -> Self {
        Self {
            config,
            in_flight: None,
            dirty: false,
        }
    }

    /// Start a save if one is warranted and none is running.
    ///
    /// Returns immediately either way. A tick that arrives while a save is in
    /// flight is *dropped* rather than queued — `dirty` stays set, so the next
    /// free tick covers the same ground.
    fn maybe_start(&mut self, room: &Room) {
        let Some(store) = self.config.store.clone() else {
            return;
        };
        if !self.dirty || self.in_flight.is_some() {
            return;
        }
        // The only part that touches the actor: `Arc` clones, measured at tens
        // of microseconds on a 2000-slot room.
        let snapshot = room.snapshot();
        let compress = self.config.compress;
        self.dirty = false;
        self.in_flight = Some(tokio::task::spawn_blocking(move || {
            // `spawn_blocking`, not a regular task: `fsync` blocks its thread,
            // and on CephFS so can every other call in here.
            let started = std::time::Instant::now();
            let bytes = snapshot.encode(compress);
            store.store(&bytes)?;
            crate::metrics::record_save(started.elapsed(), bytes.len());
            Ok(bytes.len())
        }));
    }

    /// Resolve a finished save. Never called unless one is in flight.
    async fn finished(&mut self) {
        let Some(handle) = self.in_flight.as_mut() else {
            // Nothing running: park forever and let another select branch win.
            std::future::pending::<()>().await;
            return;
        };
        let outcome = handle.await;
        self.in_flight = None;
        match outcome {
            Ok(Ok(bytes)) => tracing::debug!(bytes, "saved"),
            Ok(Err(e)) => {
                // Loudly, and then carry on. A room that dies because its
                // filesystem hiccuped is a worse outcome than a stale save.
                tracing::error!(error = %e, "save failed; the room is still running but \
                     its recovery point is stale");
                self.dirty = true;
            }
            Err(e) => tracing::error!(error = %e, "save task did not complete"),
        }
    }

    /// Final save on the way out, bounded so a hung filesystem cannot hold the
    /// process open past its grace period.
    async fn flush(&mut self, room: &Room) {
        if self.config.store.is_none() {
            return;
        }
        // Wait out an in-flight save first, so the last one to land is the
        // newest rather than whichever finished last.
        if self.in_flight.is_some() {
            let _ = tokio::time::timeout(self.config.shutdown_timeout, self.finished()).await;
        }
        self.dirty = true;
        self.maybe_start(room);
        if tokio::time::timeout(self.config.shutdown_timeout, self.finished())
            .await
            .is_err()
        {
            tracing::warn!(
                timeout = ?self.config.shutdown_timeout,
                "the final save did not finish in time; state since the last \
                 completed save is lost"
            );
        }
    }
}

pub async fn run(mut room: Room, shards: Shards, rx: mpsc::Receiver<ActorMsg>) {
    run_with_saves(&mut room, shards, rx, SaveConfig::default()).await;
}

pub async fn run_with_saves(
    room: &mut Room,
    shards: Shards,
    mut rx: mpsc::Receiver<ActorMsg>,
    save_config: SaveConfig,
) {
    // Held apart from the `Saver` so that a `Dispatcher` can borrow it while
    // `saver.dirty` is still being written — the two are independent, and
    // leaving the journal inside the config would make them look otherwise.
    let mut save_config = save_config;
    let journal = save_config.journal.take();
    let mut saver = Saver::new(save_config);
    let mut save_timer = tokio::time::interval(saver.config.interval);
    // The first tick of an `Interval` completes immediately, which would save an
    // untouched room the moment it starts.
    save_timer.tick().await;

    loop {
        // The room says when it next wants poking — only a running countdown
        // does today. With nothing pending this waits on the mailbox alone, so
        // an idle room costs nothing.
        let countdown = room
            .next_tick()
            .map(|at| Duration::from_secs_f64((at - now()).max(0.0)));

        // The bottleneck canary. A transient spike is fine; a floor that climbs
        // and does not drain means work is arriving faster than the room can
        // apply it, and every other symptom is downstream of that.
        crate::metrics::record_mailbox_depth(rx.len());

        let msg = tokio::select! {
            msg = rx.recv() => msg,
            // Waiting on a timer is not a violation of "awaits only its
            // mailbox": the point of that rule is that no *client* can make the
            // actor wait, and a clock is not a client. Neither is a save that
            // has already finished on another thread.
            _ = tokio::time::sleep(countdown.unwrap_or_default()), if countdown.is_some() => {
                let mut sink = Dispatcher::new(&shards, journal.as_ref());
                room.tick(now(), &mut sink);
                saver.dirty |= sink.dirty;
                continue;
            }
            _ = save_timer.tick() => {
                saver.maybe_start(room);
                // The journal reaches the disk on the same cadence as the save,
                // so the two agree about how much a hard kill can cost.
                if let Some(journal) = &journal {
                    journal.flush();
                }
                continue;
            }
            () = saver.finished() => continue,
        };
        let Some(msg) = msg else { break };

        let mut sink = Dispatcher::new(&shards, journal.as_ref());
        // Refresh the room's notion of "now" before it acts on anything, so a
        // command that needs a clock has a current one.
        room.tick(now(), &mut sink);

        match msg {
            ActorMsg::Connected {
                conn,
                tx,
                close,
                deflate,
                budget,
                feed,
            } => {
                shards.tell(
                    conn,
                    ShardMsg::Add {
                        conn,
                        tx,
                        close,
                        deflate,
                        budget,
                        scoped: feed == pahoa_room::FeedPolicy::Scoped,
                    },
                );
                room.on_connect_with_feed(conn, feed, &mut sink);
                push_membership(room, conn, &mut sink);
            }
            ActorMsg::Packets {
                conn,
                packets,
                bytes,
            } => {
                crate::metrics::record_client_message();
                // Once for the message, where `record_packet` below is once per
                // packet in it. Resolved before any of them are handled, so a
                // frame carrying `Connect` is charged to nobody — the same rule
                // the packet counter follows.
                crate::metrics::record_bytes_in(
                    room.client(conn)
                        .filter(|c| c.auth)
                        .map(|c| (c.team, c.slot)),
                    bytes,
                );
                for packet in packets {
                    // Attributed to the slot as it stands *now*, before this
                    // packet is handled: a `Connect` is what creates the slot,
                    // and counting it against the slot it just made would put
                    // pre-auth traffic somewhere it can never be seen for what
                    // it is.
                    let slot = room
                        .client(conn)
                        .filter(|c| c.auth)
                        .map(|c| (c.team, c.slot));
                    crate::metrics::record_packet(slot, packet.cmd());
                    room.handle(conn, packet, &mut sink);
                }
                push_membership(room, conn, &mut sink);
            }
            ActorMsg::DecodeFailed { conn, detail } => {
                tracing::info!(%conn, %detail, "dropping connection after a bad frame");
                room.on_disconnect(conn, &mut sink);
                shards.tell(
                    conn,
                    ShardMsg::Close {
                        conn,
                        reason: "protocol error",
                    },
                );
                shards.tell(conn, ShardMsg::Remove { conn });
            }
            ActorMsg::Disconnected { conn } => {
                room.on_disconnect(conn, &mut sink);
                shards.tell(conn, ShardMsg::Remove { conn });
            }
            ActorMsg::Live { reply } => {
                // The receiver may already be gone if the client hung up; that
                // is not this loop's problem.
                let _ = reply.send(crate::http::Live {
                    clients_connected: room.client_count(),
                    password_required: room.password_required(),
                });
            }
            ActorMsg::Status { reply } => {
                let _ = reply.send(crate::http::Status {
                    clients_connected: room.client_count(),
                    // Only the actor holds this: it is the saver's own notion of
                    // whether anything has changed since the last save started.
                    save_dirty: saver.dirty,
                    save_interval: saver.config.interval,
                    saving: saver.config.store.is_some(),
                    last_check_at: room.last_check_at(),
                    options: crate::http::Options {
                        hint_cost: room.options.hint_cost,
                        location_check_points: room.options.location_check_points,
                        release_mode: room.options.release_mode.as_text(),
                        collect_mode: room.options.collect_mode.as_text(),
                        remaining_mode: room.options.remaining_mode.as_text(),
                        countdown_mode: room.options.countdown_mode.as_text(),
                        item_cheat: room.options.item_cheat,
                        compatibility: room.options.compatibility,
                    },
                    // The roster question, so spectators are included: an
                    // organizer needs to see a connected spectator. Walked as
                    // `(team, slot)` — one team today, but a document listing
                    // slots alone would silently show one team's worth of a
                    // room that had more.
                    slots: room
                        .multidata()
                        .team_slots()
                        .map(|(team, number)| {
                            let info = &room.multidata().slot_info[&number];
                            let number = &number;
                            let key = (team, *number);
                            crate::http::SlotStatus {
                                team,
                                slot: *number,
                                name: info.name.clone(),
                                game: info.game.clone(),
                                connections: room.connections_for(key),
                                checks: room.checked_count(key),
                                total_checks: room.multidata().locations.count_for(*number),
                                status: room.status(key).as_text(),
                                locked: room.slot_locked(key),
                                filtered: room.filters_slot(key),
                            }
                        })
                        .collect(),
                });
            }
            ActorMsg::Tracker { reply } => {
                let _ = reply.send(room.tracker_data());
            }
            ActorMsg::Admin { command, reply } => {
                let outcome = room.admin(command, &mut sink);
                let _ = reply.send(outcome);
            }
            ActorMsg::SetSlotPassword {
                slot,
                password,
                reply,
            } => {
                let mut known = room.multidata().slot_info.contains_key(&slot);
                if known {
                    // Only meaningful while per-slot mode is in force. Setting
                    // one stores it; clearing one *removes* the key, which
                    // under fail-closed semantics bars the slot rather than
                    // opening it — the useful answer during live abuse.
                    match room.options.slot_passwords.as_mut() {
                        Some(passwords) => {
                            let set = password.is_some();
                            match password {
                                Some(password) => passwords.insert(slot, password),
                                None => passwords.remove(&slot),
                            };
                            // The fact, never the value. Clearing *locks* the
                            // slot, so this is the record that answers "why can
                            // nobody join slot 4" months later.
                            sink.journal_event(pahoa_room::JournalEvent::slot_password_changed(
                                now(),
                                slot,
                                set,
                            ));
                        }
                        // No per-slot mode to rotate within.
                        None => known = false,
                    }
                    // Deliberately no `mark_dirty`: this changes configuration,
                    // not game state, and configuration is not what a save
                    // carries.
                    tracing::info!(slot, "slot password rotated through the admin API");
                }
                let _ = reply.send(known);
            }
            ActorMsg::Filter { slot, edit, reply } => {
                let key = match slot {
                    Some(n) if !room.multidata().slot_info.contains_key(&n) => {
                        let _ = reply.send(FilterReply::UnknownSlot);
                        continue;
                    }
                    // A filter belongs to a `(team, slot)`; the path names only
                    // the slot because there is one team to name. See
                    // `pahoa_multidata::MultiData::teams`.
                    Some(n) => Some((pahoa_multidata::ONLY_TEAM, n)),
                    None => None,
                };

                let mut removed = 0;
                // What this filter is *now*, distinguishing "inherits" (`None`)
                // from "explicitly empty" — the two differ, and the difference
                // is what lets a slot opt out of the room's filter entirely.
                let existing = room.filter(key).cloned();
                let mut current = existing.clone().unwrap_or_default();
                let next: Option<Option<pahoa_room::filter::Filter>> = match edit {
                    FilterEdit::Read => None,
                    // `PUT` sets the resource, even to empty.
                    FilterEdit::Replace(rules) => {
                        current = rules;
                        Some(Some(current.clone()))
                    }
                    FilterEdit::Merge(rules) => {
                        if let Err(e) = current.merge(rules.rules) {
                            let _ = reply.send(FilterReply::Refused(e));
                            continue;
                        }
                        Some(Some(current.clone()))
                    }
                    FilterEdit::Remove(matchers) => {
                        removed = current.remove(&matchers.rules);
                        Some(Some(current.clone()))
                    }
                    // `DELETE` removes the resource, so a slot inherits again.
                    FilterEdit::Clear => {
                        current = pahoa_room::filter::Filter::default();
                        Some(None)
                    }
                };

                let own = match &next {
                    Some(value) => value.clone(),
                    None => existing,
                };
                if let Some(value) = next {
                    room.set_filter(key, value, &mut sink);
                    saver.dirty = true;
                }
                // Recomputed after the edit, because a `DELETE` on a slot puts
                // it back under the room's and the answer should say so.
                let inherited = key.is_some() && own.is_none();
                let effective = if inherited {
                    room.filter(None).cloned().unwrap_or_default()
                } else {
                    own.clone().unwrap_or_default()
                };
                let _ = reply.send(FilterReply::Ok {
                    // `null` for absent, `[]` for present-and-empty.
                    rules: own.map_or(serde_json::Value::Null, |f| f.to_json()),
                    effective: effective.to_json(),
                    inherited,
                    removed,
                });
            }
            ActorMsg::Shutdown => {
                room.shutdown(&mut sink);
                saver.dirty |= sink.dirty;
                break;
            }
        }

        saver.dirty |= sink.dirty;
        for (conn, (auth, no_text, slot)) in std::mem::take(&mut sink.updates) {
            shards.tell(
                conn,
                ShardMsg::Update {
                    conn,
                    auth,
                    no_text,
                    slot,
                },
            );
        }
    }

    saver.flush(room).await;
    tracing::info!("room actor stopped");
}

/// Tell the owning shard what it needs to filter broadcasts for this connection.
fn push_membership(room: &Room, conn: ConnId, sink: &mut Dispatcher<'_>) {
    if let Some(client) = room.client(conn) {
        sink.updates.push((
            conn,
            (
                client.auth,
                client.no_text,
                client.auth.then_some((client.team, client.slot)),
            ),
        ));
    }
}
