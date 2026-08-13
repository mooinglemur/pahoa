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

/// Membership the shard needs to expand [`Recipients`] locally.
#[derive(Debug, Clone)]
struct Member {
    tx: mpsc::Sender<Outbound>,
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
}

/// Membership flags a shard needs to filter broadcasts without consulting the
/// actor: authenticated, `NoText`, and which slot (if any) the connection holds.
pub type Membership = (bool, bool, Option<SlotKey>);

#[derive(Debug)]
pub enum ShardMsg {
    Add {
        conn: ConnId,
        tx: mpsc::Sender<Outbound>,
        deflate: Option<u8>,
        budget: ConnHandle,
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
                deflate,
                budget,
            } => {
                members.insert(
                    conn,
                    Member {
                        tx,
                        auth: false,
                        no_text: false,
                        slot: None,
                        deflate,
                        budget,
                        lagged: false,
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
                    let _ = m.tx.try_send(Outbound::Close(reason));
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
            let _ = m.tx.try_send(Outbound::Close("too slow"));
        }
    }
}
