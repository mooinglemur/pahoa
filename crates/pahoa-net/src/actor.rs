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

use crate::shard::{Outbound, ShardMsg, Shards};
use bytes::Bytes;
use pahoa_proto::{ClientPacket, ServerPacket, encode};
use pahoa_room::{CloseReason, ConnId, EffectSink, Recipients, Room};
use tokio::sync::mpsc;

pub enum ActorMsg {
    Connected {
        conn: ConnId,
        tx: mpsc::Sender<Outbound>,
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
        let frame = Bytes::from(encode(msgs));
        self.shards.tell(to, ShardMsg::Send { conn: to, frame });
    }

    fn broadcast(&mut self, to: Recipients, msgs: &[ServerPacket]) {
        if msgs.is_empty() {
            return;
        }
        // Encoded once for every recipient across every shard.
        let frame = Bytes::from(encode(msgs));
        self.shards.broadcast(to, frame);
    }

    fn close(&mut self, conn: ConnId, reason: CloseReason) {
        let text = match reason {
            CloseReason::ProtocolError(_) => "protocol error",
            CloseReason::TooSlow => "client too slow",
            CloseReason::ServerShutdown => "server shutting down",
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

pub async fn run(mut room: Room, shards: Shards, mut rx: mpsc::Receiver<ActorMsg>) {
    let mut dirty = false;

    loop {
        // The room says when it next wants poking — only a running countdown
        // does today. With nothing pending this waits on the mailbox alone, so
        // an idle room costs nothing.
        let msg = match room.next_tick() {
            None => rx.recv().await,
            Some(at) => {
                let delay = std::time::Duration::from_secs_f64((at - now()).max(0.0));
                tokio::select! {
                    msg = rx.recv() => msg,
                    // Waiting on a timer is not a violation of "awaits only its
                    // mailbox": the point of that rule is that no *client* can
                    // make the actor wait, and a clock is not a client.
                    _ = tokio::time::sleep(delay) => {
                        let mut sink = Dispatcher::new(&shards);
                        room.tick(now(), &mut sink);
                        dirty |= sink.dirty;
                        continue;
                    }
                }
            }
        };
        let Some(msg) = msg else { break };

        let mut sink = Dispatcher::new(&shards);
        // Refresh the room's notion of "now" before it acts on anything, so a
        // command that needs a clock has a current one.
        room.tick(now(), &mut sink);

        match msg {
            ActorMsg::Connected { conn, tx } => {
                shards.tell(conn, ShardMsg::Add { conn, tx });
                room.on_connect(conn, &mut sink);
                push_membership(&room, conn, &mut sink);
            }
            ActorMsg::Packets { conn, packets } => {
                for packet in packets {
                    room.handle(conn, packet, &mut sink);
                }
                push_membership(&room, conn, &mut sink);
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
            ActorMsg::Shutdown => {
                room.shutdown(&mut sink);
                break;
            }
        }

        dirty |= sink.dirty;
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

    tracing::info!(dirty, "room actor stopped");
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
