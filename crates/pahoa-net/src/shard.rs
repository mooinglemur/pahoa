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
    Send {
        conn: ConnId,
        msg: Outgoing,
    },
    Broadcast {
        to: Recipients,
        msg: Outgoing,
    },
    Close {
        conn: ConnId,
        reason: &'static str,
    },
}

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
        let _ = self.shard_of(conn).try_send(msg);
    }

    /// Hand a broadcast to every shard. Cost to the actor is K sends.
    ///
    /// The message travels **uncompressed**: each shard compresses it at most
    /// once, covering all of its own deflate connections. That is O(shards)
    /// compressions rather than O(connections), and it keeps the work off the
    /// actor — measured at ~175µs for a full 140-packet chunk, which across a
    /// mass release would be half a second of mailbox stall.
    pub fn broadcast(&self, to: Recipients, msg: Outgoing) {
        for tx in &self.txs {
            let _ = tx.try_send(ShardMsg::Broadcast {
                to: to.clone(),
                msg: msg.clone(),
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
            ShardMsg::Send { conn, msg } => {
                let mut lagged = Vec::new();
                if let Some(m) = members.get(&conn)
                    && !m.lagged
                {
                    let frame = variant(m, &msg, level, &mut deflaters, &mut Vec::new());
                    if !deliver(m, frame, &budget) {
                        lagged.push(conn);
                    }
                }
                mark_lagged(&mut members, &lagged);
            }
            ShardMsg::Broadcast { to, msg } => {
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
                for (conn, m) in recipients {
                    if m.lagged {
                        continue;
                    }
                    let frame = variant(m, &msg, level, &mut deflaters, &mut deflated);
                    if !deliver(m, frame, &budget) {
                        lagged.push(*conn);
                    }
                }
                mark_lagged(&mut members, &lagged);
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

/// Queue a frame. Never awaits. Returns false when the connection has fallen
/// too far behind and must be dropped.
///
/// **Dropping the frame and carrying on is not an option**, which is the subtle
/// part. `send_new_items` advances a slot's `send_index` as it sends, so a
/// discarded `ReceivedItems` leaves the server believing a client holds items it
/// never received — and the client cannot tell. Closing instead is safe because
/// the protocol is resumable: `Connect` resends `checked_locations` in full and
/// replays the item queue from index zero, so a lagged client reconnects into
/// correct state. Only chat scrollback is lost, which any disconnect loses.
fn deliver(member: &Member, frame: Bytes, budget: &Budget) -> bool {
    let size = frame.len();
    if !budget.reserve(&member.budget, size) {
        return false;
    }
    if member.tx.try_send(Outbound::Frame(frame)).is_err() {
        // The writer is gone or its queue is full; hand the reservation back
        // rather than leaking it, then drop the connection for the same reason.
        Budget::release(&member.budget, size);
        return false;
    }
    true
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
    fn wedged() -> (Member, mpsc::Receiver<&'static str>, mpsc::Sender<Outbound>) {
        let (tx, _held) = mpsc::channel(1);
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
            scoped: false,
        };
        // `_held` is returned so the receiver stays alive; a dropped receiver
        // would make `try_send` fail for the wrong reason.
        (member, close_rx, tx)
    }

    /// The bug in one assertion.
    ///
    /// A lagged connection's close must not be queued, because the queue is
    /// either full or — more often — accepting work its writer will never get
    /// through. Either way the client is never told, and the room forgets a
    /// socket that stays open.
    #[test]
    fn marking_a_connection_lagged_closes_it_out_of_band() {
        let (member, mut close_rx, _tx) = wedged();
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

    /// Twice is not twice as closed, and the second attempt must not panic or
    /// wedge on a full signal channel.
    #[test]
    fn a_second_lag_mark_is_harmless() {
        let (member, mut close_rx, _tx) = wedged();
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
        let (member, mut close_rx, _tx) = wedged();

        close_member(&member, "kicked");

        assert_eq!(close_rx.try_recv(), Ok("kicked"));
    }
}
