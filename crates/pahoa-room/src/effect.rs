//! How the room emits work for the transport to do.
//!
//! A **streaming** sink rather than a returned `Vec<Effect>`, and that matters
//! at scale: a 2000-slot release produces ~2,860 broadcast frames of 140
//! `PrintJSON` packets each. Collecting those before shipping any would
//! materialize the whole cascade in memory; a sink lets the transport encode
//! and fan out each chunk as it is produced, holding peak memory at one chunk.
//!
//! Recipient resolution — including the `NoText` and tracker-tag filters —
//! happens inside the room, which owns the client registry. The sink just
//! receives a resolved `&[ConnId]`.

use crate::conn::ConnId;
use crate::room::SlotKey;
use pahoa_proto::ServerPacket;

/// Who a broadcast is for, described rather than enumerated.
///
/// This is deliberately *intent*, not a resolved `&[ConnId]`. Materializing a
/// list would put an O(connections) walk in the room for every broadcast — at
/// 6000 connections and ~3,500 broadcasts in a mass release, that is over 20
/// million pushes of work the room should never be doing. Instead the transport
/// keeps the membership indexes and expands these itself, in parallel, off the
/// critical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipients {
    /// Every authenticated connection.
    All,
    /// Every authenticated connection without the `NoText` tag.
    AllText,
    /// Every connection authenticated to one slot. Co-op means this can be
    /// several.
    Slot(SlotKey),
    /// One slot's connections, minus the `NoText` ones.
    ///
    /// Distinct from [`Recipients::Slot`] because hint messages are chat and
    /// must skip `NoText` clients, while the `RoomUpdate` that accompanies them
    /// must not (`MultiServer.py:836`).
    SlotText(SlotKey),
    /// An explicit list, for the cases that genuinely need one.
    These(Vec<ConnId>),
}

/// Why the room is dropping a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The client sent something the reference server would have raised on,
    /// which drops the socket rather than replying `InvalidPacket`.
    ProtocolError(String),
    /// Outbound buffer exceeded its budget. A deliberate divergence: Python
    /// buffers without limit instead, which is unbounded memory growth.
    TooSlow,
    ServerShutdown,
}

pub trait EffectSink {
    /// Send to one connection.
    fn send(&mut self, to: ConnId, msgs: &[ServerPacket]);

    /// Send the same packets to many connections.
    ///
    /// The transport encodes once and shares the bytes; that is the whole
    /// reason this is distinct from repeated [`EffectSink::send`].
    fn broadcast(&mut self, to: Recipients, msgs: &[ServerPacket]);

    /// Drop a connection.
    fn close(&mut self, conn: ConnId, reason: CloseReason);

    /// Room state changed and should be persisted at the next save point.
    fn mark_dirty(&mut self);

    /// This connection's broadcast-filtering state changed.
    ///
    /// **Ordering is the entire point of this being an effect** rather than
    /// something the transport reads back afterwards. A transport filters
    /// [`Recipients::AllText`] against its own copy of `auth`, so if it learns
    /// about a client authenticating only after the handler returns, that
    /// client is filtered out of its own join announcement — it watches
    /// everyone else arrive and never sees itself.
    ///
    /// Defaulted to nothing, because only a real transport keeps a second copy
    /// of this state; a recorder resolves recipients against the room directly.
    fn membership_changed(
        &mut self,
        _conn: ConnId,
        _auth: bool,
        _no_text: bool,
        _slot: Option<crate::SlotKey>,
    ) {
    }
}

/// An [`EffectSink`] that records everything, for tests.
#[derive(Debug, Default)]
pub struct Recorder {
    pub events: Vec<Event>,
    pub dirty: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Send {
        to: ConnId,
        msgs: Vec<ServerPacket>,
    },
    Broadcast {
        to: Recipients,
        msgs: Vec<ServerPacket>,
    },
    Close {
        conn: ConnId,
        reason: CloseReason,
    },
}

impl EffectSink for Recorder {
    fn send(&mut self, to: ConnId, msgs: &[ServerPacket]) {
        self.events.push(Event::Send {
            to,
            msgs: msgs.to_vec(),
        });
    }

    fn broadcast(&mut self, to: Recipients, msgs: &[ServerPacket]) {
        self.events.push(Event::Broadcast {
            to,
            msgs: msgs.to_vec(),
        });
    }

    fn close(&mut self, conn: ConnId, reason: CloseReason) {
        self.events.push(Event::Close { conn, reason });
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

impl Recorder {
    /// Every packet sent to `conn`, whether directly or by broadcast.
    ///
    /// Needs the room because [`Recipients`] describes an audience rather than
    /// listing it; resolving is the room's job (and, in production, the
    /// transport's).
    pub fn packets_for(&self, conn: ConnId, room: &crate::Room) -> Vec<&ServerPacket> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Send { to, msgs } if *to == conn => Some(msgs),
                Event::Broadcast { to, msgs } if room.resolve(to).contains(&conn) => Some(msgs),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Total packets emitted, ignoring how many connections received them.
    pub fn packet_count(&self) -> usize {
        self.events
            .iter()
            .map(|e| match e {
                Event::Send { msgs, .. } | Event::Broadcast { msgs, .. } => msgs.len(),
                Event::Close { .. } => 0,
            })
            .sum()
    }

    pub fn broadcasts(&self) -> impl Iterator<Item = (&Recipients, &Vec<ServerPacket>)> {
        self.events.iter().filter_map(|e| match e {
            Event::Broadcast { to, msgs } => Some((to, msgs)),
            _ => None,
        })
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.dirty = false;
    }
}

/// A sink that counts without retaining, for the large-scale tests where
/// holding every packet would dwarf the work being measured.
#[derive(Debug, Default)]
pub struct Counter {
    pub broadcasts: usize,
    pub sends: usize,
    pub packets: usize,
    /// Largest number of packets in a single broadcast, to verify chunking.
    pub max_chunk: usize,
    pub closes: usize,
    pub dirty: bool,
}

impl EffectSink for Counter {
    fn send(&mut self, _to: ConnId, msgs: &[ServerPacket]) {
        self.sends += 1;
        self.packets += msgs.len();
        self.max_chunk = self.max_chunk.max(msgs.len());
    }

    fn broadcast(&mut self, _to: Recipients, msgs: &[ServerPacket]) {
        self.broadcasts += 1;
        self.packets += msgs.len();
        self.max_chunk = self.max_chunk.max(msgs.len());
    }

    fn close(&mut self, _conn: ConnId, _reason: CloseReason) {
        self.closes += 1;
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}
