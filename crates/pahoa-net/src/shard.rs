//! Fan-out shards.
//!
//! The room actor must not walk 6000 connections per broadcast. A mass release
//! emits ~3,500 broadcasts; iterating every connection for each would be over
//! twenty million sends on the one task that also owns all mutable state, and
//! everything else would stall behind it.
//!
//! So the actor sends each broadcast to K shard tasks — a handful of messages,
//! not thousands — and each shard expands the audience against the membership
//! it owns and writes to its own connections, in parallel, off the critical path.
//!
//! Shard assignment is `conn_id % K`, so the actor knows which shard owns a
//! connection without a lookup.

use crate::budget::{Budget, ConnHandle};
use crate::ws::Outgoing;
use crate::ws::deflate::Deflater;
use bytes::Bytes;
use pahoa_room::{ConnId, Recipients, SlotKey};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What the writer task for one connection accepts.
#[derive(Debug, Clone)]
pub enum Outbound {
    /// An already-encoded frame. Cloning is a refcount bump, which is what makes
    /// "encode once, send to many" actually cheap.
    Frame(Bytes),
    Close(&'static str),
}

/// A close that does not depend on the queue it is closing.
///
/// **This exists because the obvious version deadlocks against itself.** A
/// connection is marked lagged precisely *because* its outbound queue
/// overflowed; queuing the close onto that same full queue therefore fails in
/// exactly the case the close exists for, and the room then forgets a client
/// that never learned it was dropped. The socket stays open, the player keeps
/// typing into a room that is no longer listening, and neither side can tell.
///
/// A capacity of one is the whole design: a second close is redundant, so a
/// full channel here means one is already on its way and dropping the duplicate
/// is correct rather than lossy.
pub type CloseSignal = mpsc::Sender<&'static str>;

/// Close `member`, preferring the ordered path and guaranteeing the outcome.
///
/// The queue is tried first so that anything already queued for this connection
/// still reaches it — an admin kick sends the player an explanation immediately
/// before closing, and jumping the queue would drop it. The out-of-band signal
/// is the fallback, and it only fires when the queue is full, which is when
/// those queued frames were never going to be delivered anyway.
fn close_member(member: &Member, reason: &'static str) {
    if member.tx.try_send(Outbound::Close(reason)).is_ok() {
        return;
    }
    // Full or gone. `try_send` failing here means either a close is already
    // pending or the writer has stopped; both end the connection.
    force_close(member, reason);
}

/// Close without going through the outbound queue at all.
///
/// For a connection already known not to be keeping up, and the distinction
/// from [`close_member`] is the whole fix rather than a refinement of it.
/// A queue that *accepts* a close is not a queue that will *deliver* one: the
/// common way to lag is to exhaust the byte budget while the writer sits in a
/// `write_all` against a peer that has stopped reading. The queue then has room,
/// the ordered close is happily accepted, and it waits behind frames that will
/// never be written. Every path that queues the close therefore succeeds and
/// nothing reaches the socket — which is exactly the state that looked, from
/// the room's side, like a completed disconnect.
///
/// A client that is not draining cannot read a courtesy close frame anyway, so
/// nothing is lost by skipping the queue for it.
fn force_close(member: &Member, reason: &'static str) {
    let _ = member.close.try_send(reason);
}

/// Membership the shard needs to expand [`Recipients`] locally.
#[derive(Debug, Clone)]
struct Member {
    tx: mpsc::Sender<Outbound>,
    /// The escape hatch for closing a connection whose queue is full. See
    /// [`CloseSignal`].
    close: CloseSignal,
    auth: bool,
    no_text: bool,
    slot: Option<SlotKey>,
    /// The window size this connection negotiated, if it took deflate at all.
    ///
    /// Carried as a size rather than a flag because a client may cap our window
    /// below the default and will then inflate with that smaller window;
    /// compressing with a larger one emits back-references it cannot resolve.
    deflate: Option<u8>,
    /// This connection's share of the outbound byte budget, shared with its
    /// writer task, which releases bytes as they reach the socket.
    budget: ConnHandle,
    /// Already dropped for falling behind; nothing more is queued for it.
    lagged: bool,
    /// The slot's send filter, if it has one.
    ///
    /// Held per connection rather than looked up per slot because the shard has
    /// no slot table beyond `by_slot` and the check runs once per recipient per
    /// broadcast — a pointer compare and at most a few rule tests, on the path
    /// a mass release walks. Shared behind an `Arc` so pushing a filter to a
    /// slot's six connections copies nothing.
    filter: Option<Arc<pahoa_room::filter::Filter>>,
    /// This connection arrived on the scoped port and receives only what
    /// concerns its own slot.
    ///
    /// Set once, on `Add`, and never by an `Update` — the policy comes from the
    /// port, and nothing a client sends may lower it. See
    /// `docs/scoped-feed.md`.
    scoped: bool,
}

/// Membership flags a shard needs to filter broadcasts without consulting the
/// actor: authenticated, `NoText`, and which slot (if any) the connection holds.
pub type Membership = (bool, bool, Option<SlotKey>);

#[derive(Debug)]
pub enum ShardMsg {
    Add {
        conn: ConnId,
        tx: mpsc::Sender<Outbound>,
        close: CloseSignal,
        deflate: Option<u8>,
        budget: ConnHandle,
        /// From the port this connection arrived on, and fixed for its life.
        scoped: bool,
    },
    Remove {
        conn: ConnId,
    },
    /// Authentication or tag change; shards keep these so they can filter
    /// without asking the actor.
    Update {
        conn: ConnId,
        auth: bool,
        no_text: bool,
        slot: Option<SlotKey>,
    },
    /// Replace this connection's send filter. Separate from `Update` because a
    /// filter changes for reasons that have nothing to do with membership — an
    /// operator editing it mid-game — and because the room pushes it to every
    /// connection of a slot at once.
    SetFilter {
        conn: ConnId,
        filter: Option<Arc<pahoa_room::filter::Filter>>,
    },
    Send {
        conn: ConnId,
        msg: Outgoing,
        /// What a filter rule may name this frame, if anything. `None` is
        /// unfilterable and always delivered — see
        /// `pahoa_room::filter::outbound_tag`.
        tag: Option<OutTag>,
    },
    Broadcast {
        to: Recipients,
        msg: Outgoing,
        tag: Option<OutTag>,
    },
    Close {
        conn: ConnId,
        reason: &'static str,
    },
}

/// What a filter rule can match an outbound frame against.
///
/// Resolved once by the actor, from the packets, before they are encoded — the
/// shard sees only bytes by then. `Arc` because one broadcast hands the same
/// tag to every shard.
pub type OutTag = Arc<(pahoa_room::filter::Kind, Vec<String>)>;

/// Handles for the actor to talk to its shards.
#[derive(Debug, Clone)]
pub struct Shards {
    txs: Vec<mpsc::Sender<ShardMsg>>,
}

impl Shards {
    pub fn spawn(count: usize, queue_depth: usize, compression_level: u32, budget: Budget) -> Self {
        let mut txs = Vec::with_capacity(count);
        for index in 0..count {
            let (tx, rx) = mpsc::channel(queue_depth);
            txs.push(tx);
            tokio::spawn(run_shard(index, rx, compression_level, budget.clone()));
        }
        Self { txs }
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    fn shard_of(&self, conn: ConnId) -> &mpsc::Sender<ShardMsg> {
        &self.txs[conn.0 as usize % self.txs.len()]
    }

    /// Route to the one shard that owns this connection.
    pub fn tell(&self, conn: ConnId, msg: ShardMsg) {
        // try_send, never await: the actor blocking on a shard would reintroduce
        // exactly the head-of-line stall shards exist to prevent.
        // A dropped `Remove` is not merely a lost message: it strands the
        // member, and with it the outbound budget it still holds, for the life
        // of the process. Say so rather than discarding it silently.
        if let Err(e) = self.shard_of(conn).try_send(msg) {
            let dropped = match &e {
                mpsc::error::TrySendError::Full(m) | mpsc::error::TrySendError::Closed(m) => m,
            };
            if matches!(dropped, ShardMsg::Remove { .. }) {
                tracing::warn!(%conn, "shard mailbox full; a connection's removal was dropped");
            }
        }
    }

    /// Hand a broadcast to every shard. Cost to the actor is K sends.
    ///
    /// The message travels **uncompressed**: each shard compresses it at most
    /// once, covering all of its own deflate connections. That is O(shards)
    /// compressions rather than O(connections), and it keeps the work off the
    /// actor — measured at ~175µs for a full 140-packet chunk, which across a
    /// mass release would be half a second of mailbox stall.
    pub fn broadcast(&self, to: Recipients, msg: Outgoing, tag: Option<OutTag>) {
        for tx in &self.txs {
            let _ = tx.try_send(ShardMsg::Broadcast {
                to: to.clone(),
                msg: msg.clone(),
                tag: tag.clone(),
            });
        }
    }
}

async fn run_shard(index: usize, mut rx: mpsc::Receiver<ShardMsg>, level: u32, budget: Budget) {
    let mut members: HashMap<ConnId, Member> = HashMap::new();
    // Slot membership, so `Recipients::Slot` needs no scan.
    let mut by_slot: HashMap<SlotKey, Vec<ConnId>> = HashMap::new();
    // One compressor per negotiated window size, shareable across every
    // connection using that size precisely because `server_no_context_takeover`
    // makes it stateless. In practice this holds exactly one entry — a client
    // capping our window below the default is rare — so the linear scan is
    // cheaper than hashing, and it is bounded at the seven legal sizes.
    let mut deflaters: Vec<(u8, Deflater)> = Vec::new();
    // One generator per shard, seeded from the shard index so two shards do not
    // draw the same sequence. Independent streams are fine: a sampling rule is
    // a proportion over many messages, not a schedule.
    let mut sampler = pahoa_room::filter::Sampler::new(0x9E37_79B9_7F4A_7C15 ^ index as u64);

    while let Some(msg) = rx.recv().await {
        match msg {
            ShardMsg::Add {
                conn,
                tx,
                close,
                deflate,
                budget,
                scoped,
            } => {
                members.insert(
                    conn,
                    Member {
                        tx,
                        close,
                        auth: false,
                        no_text: false,
                        slot: None,
                        deflate,
                        budget,
                        lagged: false,
                        filter: None,
                        scoped,
                    },
                );
            }
            ShardMsg::Remove { conn } => {
                if let Some(m) = members.remove(&conn) {
                    // Whatever it never drained goes back to the global budget,
                    // or every disconnect leaks and the room eventually refuses
                    // to send anything at all.
                    Budget::release_all(&m.budget);
                    if let Some(slot) = m.slot
                        && let Some(list) = by_slot.get_mut(&slot)
                    {
                        list.retain(|c| *c != conn);
                    }
                }
            }
            ShardMsg::Update {
                conn,
                auth,
                no_text,
                slot,
            } => {
                if let Some(m) = members.get_mut(&conn) {
                    if m.slot != slot {
                        if let Some(old) = m.slot
                            && let Some(list) = by_slot.get_mut(&old)
                        {
                            list.retain(|c| *c != conn);
                        }
                        if let Some(new) = slot {
                            by_slot.entry(new).or_default().push(conn);
                        }
                        m.slot = slot;
                    }
                    m.auth = auth;
                    m.no_text = no_text;
                }
            }
            ShardMsg::SetFilter { conn, filter } => {
                if let Some(m) = members.get_mut(&conn) {
                    m.filter = filter;
                }
            }
            ShardMsg::Send { conn, msg, tag } => {
                let mut lagged = Vec::new();
                let mut gone = Vec::new();
                if let Some(m) = members.get(&conn)
                    && !m.lagged
                    && !filtered(m, tag.as_deref(), &mut sampler)
                {
                    let frame = variant(m, &msg, level, &mut deflaters, &mut Vec::new());
                    match deliver(m, frame, &budget) {
                        Delivery::Sent => {}
                        Delivery::Behind => lagged.push(conn),
                        Delivery::Gone => gone.push(conn),
                    }
                }
                mark_lagged(&mut members, &lagged);
                mark_gone(&mut members, &gone);
            }
            ShardMsg::Broadcast { to, msg, tag } => {
                // Memoized across this whole broadcast, per window size, and
                // never built at all unless a connection here wants it.
                let mut deflated: Vec<(u8, Bytes)> = Vec::new();
                let mut recipients: Vec<(&ConnId, &Member)> = Vec::new();
                match &to {
                    Recipients::All => {
                        recipients.extend(members.iter().filter(|(_, m)| m.auth));
                    }
                    Recipients::AllText => {
                        recipients.extend(members.iter().filter(|(_, m)| m.auth && !m.no_text));
                    }
                    Recipients::Slot(key) => {
                        recipients.extend(
                            by_slot
                                .get(key)
                                .into_iter()
                                .flatten()
                                .filter_map(|c| members.get_key_value(c)),
                        );
                    }
                    Recipients::SlotText(key) => {
                        recipients.extend(
                            by_slot
                                .get(key)
                                .into_iter()
                                .flatten()
                                .filter_map(|c| members.get_key_value(c))
                                .filter(|(_, m)| !m.no_text),
                        );
                    }
                    Recipients::AllTextAbout(key) => {
                        recipients.extend(members.iter().filter(|(_, m)| {
                            m.auth && !m.no_text && (!m.scoped || m.slot == Some(*key))
                        }));
                    }
                    Recipients::AllTextFull => {
                        recipients.extend(
                            members
                                .iter()
                                .filter(|(_, m)| m.auth && !m.no_text && !m.scoped),
                        );
                    }
                    Recipients::SlotScopedText(key) => {
                        recipients.extend(
                            by_slot
                                .get(key)
                                .into_iter()
                                .flatten()
                                .filter_map(|c| members.get_key_value(c))
                                .filter(|(_, m)| !m.no_text && m.scoped),
                        );
                    }
                    Recipients::These(list) => {
                        recipients.extend(list.iter().filter_map(|c| members.get_key_value(c)));
                    }
                }

                let mut lagged = Vec::new();
                let mut gone = Vec::new();
                for (conn, m) in recipients {
                    if m.lagged {
                        continue;
                    }
                    // **After the audience, before the frame.** A filtered
                    // recipient costs a rule test and nothing else: the shared
                    // buffer is still compressed once for everyone who takes
                    // it, which is the property that makes filtering per
                    // recipient affordable at all.
                    if filtered(m, tag.as_deref(), &mut sampler) {
                        continue;
                    }
                    let frame = variant(m, &msg, level, &mut deflaters, &mut deflated);
                    match deliver(m, frame, &budget) {
                        Delivery::Sent => {}
                        Delivery::Behind => lagged.push(*conn),
                        Delivery::Gone => gone.push(*conn),
                    }
                }
                mark_lagged(&mut members, &lagged);
                mark_gone(&mut members, &gone);
            }
            ShardMsg::Close { conn, reason } => {
                if let Some(m) = members.get(&conn) {
                    // An admin kick most often lands on a client that is
                    // already struggling — which is exactly when a queued close
                    // would be dropped and the kick would report success while
                    // the client stayed connected.
                    close_member(m, reason);
                }
            }
        }
    }
    tracing::debug!(shard = index, "shard stopped");
}

/// Whether this recipient's filter drops this frame.
///
/// `None` for any of the three means deliver: an untagged frame is one no rule
/// can name — everything carrying progression, among others — a member with no
/// filter takes everything, and a connection with no slot yet is nobody's
/// filter to apply. All are the common case and cost one comparison.
fn filtered(
    member: &Member,
    tag: Option<&(pahoa_room::filter::Kind, Vec<String>)>,
    sampler: &mut pahoa_room::filter::Sampler,
) -> bool {
    let (Some(filter), Some((kind, labels)), Some(key)) = (&member.filter, tag, member.slot) else {
        return false;
    };
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    filter.drops(
        key,
        pahoa_room::filter::Direction::ToSlot,
        *kind,
        &labels,
        &mut || sampler.roll(),
    )
}

/// Pick the frame this connection wants, compressing lazily.
///
/// `deflated` memoizes across the recipients of one broadcast, so a shard with
/// 800 deflate connections compresses once rather than 800 times. That memo is
/// only sound because `server_no_context_takeover` makes the output a pure
/// function of `(payload, window bits)` — with context takeover every
/// connection's compressor would be at a different point in its own stream.
///
/// Keyed on the window size, because that is the other half of the input.
fn variant(
    member: &Member,
    msg: &Outgoing,
    level: u32,
    deflaters: &mut Vec<(u8, Deflater)>,
    deflated: &mut Vec<(u8, Bytes)>,
) -> Bytes {
    let Some(bits) = member.deflate else {
        return msg.plain();
    };
    if let Some((_, frame)) = deflated.iter().find(|(b, _)| *b == bits) {
        return frame.clone();
    }
    let deflater = match deflaters.iter().position(|(b, _)| *b == bits) {
        Some(index) => &mut deflaters[index].1,
        None => {
            deflaters.push((bits, Deflater::new(level, bits)));
            &mut deflaters.last_mut().expect("just pushed").1
        }
    };
    let frame = msg.deflated(deflater);
    deflated.push((bits, frame.clone()));
    frame
}

/// What happened to one frame.
///
/// The distinction between the last two is the whole point. Both end the
/// connection's participation, but only [`Delivery::Behind`] is a *judgment*
/// about the client, and only that one should be counted and announced as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sent,
    /// The client is not keeping up: it is over budget, or its writer's queue
    /// is full. This is the case the lag policy exists for.
    Behind,
    /// The writer task has already exited, so this connection is over — the
    /// peer hung up, or it was closed. Nothing is owed to it and nothing is
    /// wrong with it; the shard has simply not seen its `Remove` yet.
    Gone,
}

/// Queue a frame. Never awaits.
///
/// **Dropping the frame and carrying on is not an option**, which is the subtle
/// part. `send_new_items` advances a slot's `send_index` as it sends, so a
/// discarded `ReceivedItems` leaves the server believing a client holds items it
/// never received — and the client cannot tell. Closing instead is safe because
/// the protocol is resumable: `Connect` resends `checked_locations` in full and
/// replays the item queue from index zero, so a lagged client reconnects into
/// correct state. Only chat scrollback is lost, which any disconnect loses.
fn deliver(member: &Member, frame: Bytes, budget: &Budget) -> Delivery {
    let size = frame.len();
    if !budget.reserve(&member.budget, size) {
        return Delivery::Behind;
    }
    match member.tx.try_send(Outbound::Frame(frame)) {
        Ok(()) => {
            // Counted only once the frame is the writer's problem. A delivery
            // refused for lag or a closed writer never reached a socket and
            // must not read as though it had.
            crate::metrics::record_delivery(member.slot, size);
            Delivery::Sent
        }
        Err(e) => {
            // Hand the reservation back rather than leaking it, whichever of
            // the two this was.
            Budget::release(&member.budget, size);
            match e {
                mpsc::error::TrySendError::Full(_) => Delivery::Behind,
                mpsc::error::TrySendError::Closed(_) => Delivery::Gone,
            }
        }
    }
}

/// Stop sending to connections whose writer has already exited.
///
/// **This is not a lag disconnect and must not be reported as one.** Every
/// ordinary disconnect passes through here: the writer task drops `out_rx` the
/// moment the peer hangs up, and any broadcast between then and the actor's
/// `Remove` arriving finds a closed channel. Counting those made
/// `lag_disconnects` — documented as "should be zero in a healthy room" — climb
/// once per disconnect, and put an `INFO` line accusing a client of being too
/// slow into the log for what was a clean goodbye. On a room with reconnect
/// churn that is the whole log.
///
/// The flag is reused because the effect on the shard is identical: send it
/// nothing further and wait for `Remove`. There is nothing to close — the
/// writer is what closed.
fn mark_gone(members: &mut HashMap<ConnId, Member>, gone: &[ConnId]) {
    for conn in gone {
        if let Some(m) = members.get_mut(conn)
            && !m.lagged
        {
            m.lagged = true;
            tracing::debug!(%conn, "writer already gone; awaiting removal");
        }
    }
}

/// Close out the connections that could not keep up.
fn mark_lagged(members: &mut HashMap<ConnId, Member>, lagged: &[ConnId]) {
    for conn in lagged {
        if let Some(m) = members.get_mut(conn) {
            if m.lagged {
                continue;
            }
            m.lagged = true;
            crate::metrics::record_lag_disconnect();
            tracing::info!(%conn, "dropping a connection that cannot keep up");
            // Out of band, unconditionally. This connection is lagged, so its
            // writer is by definition behind — queuing the close would put it
            // after work that is not moving. See `force_close`.
            force_close(m, "too slow");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A member whose outbound queue is already full, which is the state every
    /// interesting case here starts from.
    fn wedged() -> (
        Member,
        mpsc::Receiver<&'static str>,
        mpsc::Receiver<Outbound>,
    ) {
        let (tx, held) = mpsc::channel(1);
        // Fill it, so any `try_send(Outbound::Close)` must fail.
        tx.try_send(Outbound::Frame(Bytes::from_static(b"x")))
            .expect("first send fits");
        let (close_tx, close_rx) = mpsc::channel(1);
        let member = Member {
            tx: tx.clone(),
            close: close_tx,
            auth: true,
            no_text: false,
            slot: Some((0, 1)),
            deflate: None,
            budget: ConnHandle::default(),
            lagged: false,
            filter: None,
            scoped: false,
        };
        // `held` is *returned*, not merely bound: a receiver dropped here would
        // close the channel, and `try_send` would then fail for the wrong
        // reason entirely — "this connection is gone" rather than "this
        // connection is behind". Those are the two cases these tests exist to
        // tell apart.
        (member, close_rx, held)
    }

    /// The bug in one assertion.
    ///
    /// A lagged connection's close must not be queued, because the queue is
    /// either full or — more often — accepting work its writer will never get
    /// through. Either way the client is never told, and the room forgets a
    /// socket that stays open.
    #[test]
    fn marking_a_connection_lagged_closes_it_out_of_band() {
        let (member, mut close_rx, _out_rx) = wedged();
        let mut members = HashMap::from([(ConnId(1), member)]);

        mark_lagged(&mut members, &[ConnId(1)]);

        assert_eq!(
            close_rx.try_recv(),
            Ok("too slow"),
            "a lagged connection was not closed out of band, so the client is \
             never told and its socket stays open"
        );
        assert!(members[&ConnId(1)].lagged);
    }

    /// **The subtle half, and the one the cluster actually hit.**
    ///
    /// The queue here has plenty of room, so a queued close would be accepted —
    /// and would then wait behind frames whose `write_all` is blocked against a
    /// peer that stopped reading. Accepting is not delivering. A lagged
    /// connection must therefore skip the queue *even when the queue looks
    /// healthy*, which is exactly what a naive reading of "the queue was full"
    /// would get wrong.
    #[test]
    fn a_lagged_close_skips_the_queue_even_when_the_queue_has_room() {
        let (close_tx, mut close_rx) = mpsc::channel(1);
        let (tx, mut rx) = mpsc::channel(64);
        let member = Member {
            tx,
            close: close_tx,
            auth: true,
            no_text: false,
            slot: Some((0, 1)),
            deflate: None,
            budget: ConnHandle::default(),
            lagged: false,
            filter: None,
            scoped: false,
        };
        let mut members = HashMap::from([(ConnId(1), member)]);

        mark_lagged(&mut members, &[ConnId(1)]);

        assert_eq!(
            close_rx.try_recv(),
            Ok("too slow"),
            "the close was queued instead of forced; a wedged writer will never \
             reach it and the client stays connected to a room that forgot it"
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing should have been queued for a connection that is not draining"
        );
    }

    /// **An ordinary goodbye is not a lag disconnect.**
    ///
    /// The writer task drops `out_rx` the moment its peer hangs up, so any
    /// broadcast between then and the actor's `Remove` finds a closed channel.
    /// Reading that as "this client cannot keep up" accused every departing
    /// player of being too slow: on the dev cluster a room with reconnect churn
    /// logged one such line per disconnect and drove `lag_disconnects` — which
    /// the metric's own help text says should be zero in a healthy room —
    /// straight up, hiding the real congestion it exists to report.
    #[test]
    fn a_closed_writer_is_not_a_lagging_client() {
        let (close_tx, mut close_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel(64);
        drop(rx); // The writer task has exited; this is every clean disconnect.
        let member = Member {
            tx,
            close: close_tx,
            auth: true,
            no_text: false,
            slot: Some((0, 1)),
            deflate: None,
            budget: ConnHandle::default(),
            lagged: false,
            filter: None,
            scoped: false,
        };
        let budget = Budget::new(1 << 20, 1 << 16);

        assert_eq!(
            deliver(&member, Bytes::from_static(b"hello"), &budget),
            Delivery::Gone,
            "a closed queue is a finished connection, not a slow one"
        );
        // The reservation handback is asserted in `budget`'s own tests, which
        // serialize against the process-wide counters this one must not touch.

        let mut members = HashMap::from([(ConnId(1), member)]);
        mark_gone(&mut members, &[ConnId(1)]);

        assert!(
            members[&ConnId(1)].lagged,
            "the shard must stop sending to it and wait for its removal"
        );
        assert!(
            close_rx.try_recv().is_err(),
            "there is nothing to close: the writer is what closed"
        );
    }

    /// The other half of the distinction — a *full* queue really is a client
    /// that is not keeping up, and must still be treated as one.
    #[test]
    fn a_full_writer_queue_is_still_a_lagging_client() {
        let (member, _close_rx, _tx) = wedged();
        let budget = Budget::new(1 << 20, 1 << 16);

        assert_eq!(
            deliver(&member, Bytes::from_static(b"hello"), &budget),
            Delivery::Behind,
            "a full queue is the case the lag policy exists for"
        );
    }

    /// Twice is not twice as closed, and the second attempt must not panic or
    /// wedge on a full signal channel.
    #[test]
    fn a_second_lag_mark_is_harmless() {
        let (member, mut close_rx, _out_rx) = wedged();
        let mut members = HashMap::from([(ConnId(1), member)]);

        mark_lagged(&mut members, &[ConnId(1)]);
        mark_lagged(&mut members, &[ConnId(1)]);

        assert_eq!(close_rx.try_recv(), Ok("too slow"));
        // One signal is enough; the capacity-1 channel drops the duplicate.
        assert!(close_rx.try_recv().is_err());
    }

    /// A kick prefers the ordered path, so that the explanation the room just
    /// queued for the player still reaches them.
    #[test]
    fn a_kick_uses_the_queue_when_it_has_room() {
        let (close_tx, mut close_rx) = mpsc::channel(1);
        let (tx, mut rx) = mpsc::channel(4);
        let member = Member {
            tx,
            close: close_tx,
            auth: true,
            no_text: false,
            slot: Some((0, 1)),
            deflate: None,
            budget: ConnHandle::default(),
            lagged: false,
            filter: None,
            scoped: false,
        };

        close_member(&member, "kicked");

        assert!(
            matches!(rx.try_recv(), Ok(Outbound::Close("kicked"))),
            "an ordered close should travel with the frames it must follow"
        );
        assert!(
            close_rx.try_recv().is_err(),
            "the out-of-band path is a fallback, not the default"
        );
    }

    /// ...but falls back when it cannot, rather than reporting a disconnect it
    /// did not perform.
    #[test]
    fn a_kick_falls_back_out_of_band_when_the_queue_is_full() {
        let (member, mut close_rx, _out_rx) = wedged();

        close_member(&member, "kicked");

        assert_eq!(close_rx.try_recv(), Ok("kicked"));
    }
}
