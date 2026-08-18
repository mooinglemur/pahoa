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
        /// The deflate window this connection negotiated, if any. Decides which
        /// variant of a broadcast its shard hands it.
        deflate: Option<u8>,
        /// This connection's share of the outbound byte budget.
        budget: crate::budget::ConnHandle,
    },
    Packets {
        conn: ConnId,
        packets: Vec<ClientPacket>,
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
    Shutdown,
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
}

impl<'a> Dispatcher<'a> {
    fn new(shards: &'a Shards) -> Self {
        Self {
            shards,
            dirty: false,
            updates: Vec::new(),
        }
    }
}

impl EffectSink for Dispatcher<'_> {
    fn send(&mut self, to: ConnId, msgs: &[ServerPacket]) {
        if msgs.is_empty() {
            return;
        }
        let msg = Outgoing::text(encode(msgs).as_bytes());
        self.shards.tell(to, ShardMsg::Send { conn: to, msg });
    }

    fn broadcast(&mut self, to: Recipients, msgs: &[ServerPacket]) {
        if msgs.is_empty() {
            return;
        }
        // Encoded and framed once for every recipient across every shard.
        // Compression deliberately happens further out, in the shards — see
        // `Shards::broadcast`.
        let msg = Outgoing::text(encode(msgs).as_bytes());
        self.shards.broadcast(to, msg);
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
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            store: None,
            interval: Duration::from_secs(60),
            compress: true,
            shutdown_timeout: Duration::from_secs(10),
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
                let mut sink = Dispatcher::new(&shards);
                room.tick(now(), &mut sink);
                saver.dirty |= sink.dirty;
                continue;
            }
            _ = save_timer.tick() => {
                saver.maybe_start(room);
                continue;
            }
            () = saver.finished() => continue,
        };
        let Some(msg) = msg else { break };

        let mut sink = Dispatcher::new(&shards);
        // Refresh the room's notion of "now" before it acts on anything, so a
        // command that needs a clock has a current one.
        room.tick(now(), &mut sink);

        match msg {
            ActorMsg::Connected {
                conn,
                tx,
                deflate,
                budget,
            } => {
                shards.tell(
                    conn,
                    ShardMsg::Add {
                        conn,
                        tx,
                        deflate,
                        budget,
                    },
                );
                room.on_connect(conn, &mut sink);
                push_membership(room, conn, &mut sink);
            }
            ActorMsg::Packets { conn, packets } => {
                crate::metrics::record_client_message();
                for packet in packets {
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
                    slots: room
                        .multidata()
                        .player_slots()
                        .map(|(number, info)| {
                            let key = (0, *number);
                            crate::http::SlotStatus {
                                slot: *number,
                                name: info.name.clone(),
                                game: info.game.clone(),
                                connections: room.connections_for(key),
                                checks: room.checked_count(key),
                                total_checks: room.multidata().locations.count_for(*number),
                                status: room.status(key).as_text(),
                            }
                        })
                        .collect(),
                });
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
                let known = room.multidata().slot_info.contains_key(&slot);
                if known {
                    match password {
                        Some(password) => room.options.slot_passwords.insert(slot, password),
                        None => room.options.slot_passwords.remove(&slot),
                    };
                    // Deliberately no `mark_dirty`: this changes configuration,
                    // not game state, and configuration is not what a save
                    // carries.
                    tracing::info!(slot, "slot password rotated through the admin API");
                }
                let _ = reply.send(known);
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
