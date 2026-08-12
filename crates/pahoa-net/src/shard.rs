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
}

/// Membership flags a shard needs to filter broadcasts without consulting the
/// actor: authenticated, `NoText`, and which slot (if any) the connection holds.
pub type Membership = (bool, bool, Option<SlotKey>);

#[derive(Debug)]
pub enum ShardMsg {
    Add {
        conn: ConnId,
        tx: mpsc::Sender<Outbound>,
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
        frame: Bytes,
    },
    Broadcast {
        to: Recipients,
        frame: Bytes,
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
    pub fn spawn(count: usize, queue_depth: usize) -> Self {
        let mut txs = Vec::with_capacity(count);
        for index in 0..count {
            let (tx, rx) = mpsc::channel(queue_depth);
            txs.push(tx);
            tokio::spawn(run_shard(index, rx));
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
    pub fn broadcast(&self, to: Recipients, frame: Bytes) {
        for tx in &self.txs {
            let _ = tx.try_send(ShardMsg::Broadcast {
                to: to.clone(),
                frame: frame.clone(),
            });
        }
    }
}

async fn run_shard(index: usize, mut rx: mpsc::Receiver<ShardMsg>) {
    let mut members: HashMap<ConnId, Member> = HashMap::new();
    // Slot membership, so `Recipients::Slot` needs no scan.
    let mut by_slot: HashMap<SlotKey, Vec<ConnId>> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            ShardMsg::Add { conn, tx } => {
                members.insert(
                    conn,
                    Member {
                        tx,
                        auth: false,
                        no_text: false,
                        slot: None,
                    },
                );
            }
            ShardMsg::Remove { conn } => {
                if let Some(m) = members.remove(&conn)
                    && let Some(slot) = m.slot
                    && let Some(list) = by_slot.get_mut(&slot)
                {
                    list.retain(|c| *c != conn);
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
            ShardMsg::Send { conn, frame } => {
                if let Some(m) = members.get(&conn) {
                    deliver(m, Outbound::Frame(frame));
                }
            }
            ShardMsg::Broadcast { to, frame } => match &to {
                Recipients::All => {
                    for m in members.values().filter(|m| m.auth) {
                        deliver(m, Outbound::Frame(frame.clone()));
                    }
                }
                Recipients::AllText => {
                    for m in members.values().filter(|m| m.auth && !m.no_text) {
                        deliver(m, Outbound::Frame(frame.clone()));
                    }
                }
                Recipients::Slot(key) => {
                    for conn in by_slot.get(key).into_iter().flatten() {
                        if let Some(m) = members.get(conn) {
                            deliver(m, Outbound::Frame(frame.clone()));
                        }
                    }
                }
                Recipients::These(list) => {
                    for conn in list {
                        if let Some(m) = members.get(conn) {
                            deliver(m, Outbound::Frame(frame.clone()));
                        }
                    }
                }
            },
            ShardMsg::Close { conn, reason } => {
                if let Some(m) = members.get(&conn) {
                    let _ = m.tx.try_send(Outbound::Close(reason));
                }
            }
        }
    }
    tracing::debug!(shard = index, "shard stopped");
}

/// Never awaits.
///
/// A full queue means the client cannot keep up. Python buffers without limit
/// here, which is unbounded memory growth; dropping the frame and letting the
/// writer notice is bounded instead. This is safe because the protocol is
/// resumable — `ReceivedItems` is index-addressed and `Connect` resyncs
/// `checked_locations` — so a lagging client that reconnects lands in a correct
/// state. Only chat scrollback is lost, which any disconnect already loses.
fn deliver(member: &Member, out: Outbound) {
    if member.tx.try_send(out).is_err() {
        tracing::debug!("dropping frame for a connection that cannot keep up");
    }
}
