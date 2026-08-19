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

    /// Everyone on a full feed, plus the connections of the slot this is
    /// *about*.
    ///
    /// For messages attributable to one slot — a join, a part, a cheat-console
    /// grant. A scoped connection wants these for itself and not for the other
    /// two thousand slots, which is most of what makes its feed quiet. See
    /// `docs/scoped-feed.md`.
    AllTextAbout(SlotKey),
    /// Only connections on a full feed.
    ///
    /// The item firehose, which scoped connections receive as a per-slot
    /// subset through [`Recipients::SlotScopedText`] instead.
    AllTextFull,
    /// Only the *scoped* connections of one slot.
    ///
    /// Deliberately not [`Recipients::SlotText`]: a full-feed connection on the
    /// same slot already had this message from the broadcast, and sending it
    /// again would double every item it receives.
    SlotScopedText(SlotKey),

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
    /// An administrator disconnected this slot.
    ///
    /// Carries no reason: the shard's close message holds a `&'static str`, and
    /// an operator's sentence reaches the client as a `PrintJSON` sent just
    /// before this — which keeps an allocation off the broadcast path for the
    /// sake of one rare command.
    Kicked,
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

    /// One location was checked, for the room's durable history.
    ///
    /// An effect rather than something the room writes, for the usual reason —
    /// the room owns no files and has no clock beyond what it is handed. The
    /// transport decides whether anyone is listening, and a sink that is not
    /// journalling does nothing at all here.
    ///
    /// **Deliberately carries ids, not names.** A release pushes every location
    /// a slot owns through this in one burst — 341,851 of them on a 2000-slot
    /// room — and this runs on the task that owns all room state. Resolving
    /// four names per record would put the allocations on that thread; the
    /// record is `Copy`, and whatever consumes it resolves names on its own
    /// time. See `docs/journal.md`.
    fn journal_check(&mut self, _record: CheckRecord) {}

    /// Anything else the room's history records.
    ///
    /// Separate from [`EffectSink::journal_check`] because the two live in
    /// different volume regimes and want different trades. A check is emitted
    /// 341,851 times by a single release, so it is `Copy` and resolves nothing;
    /// these are emitted when a person does something, so they can afford to
    /// own their data and be shaped on the spot.
    fn journal_event(&mut self, _event: JournalEvent) {}
}

/// A low-volume journal record, already shaped as the line that will be written.
///
/// Holding the rendered object rather than a typed enum is deliberate: the
/// journal's contract *is* its JSON shape, so building the object is building
/// the output, and a reader that understands `type` can be extended without the
/// writer growing an arm per event. The constructors below are where the shape
/// is decided, and are the only way to make one.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEvent(serde_json::Value);

impl JournalEvent {
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// What this event is, for a reader dispatching on `type`.
    pub fn kind(&self) -> &str {
        self.0["type"].as_str().unwrap_or("")
    }

    fn new(kind: &str, mut fields: serde_json::Map<String, serde_json::Value>) -> Self {
        fields.insert("type".to_string(), kind.into());
        Self(serde_json::Value::Object(fields))
    }

    /// The room's effective options, written at start and after any change.
    ///
    /// **Carries password *modes*, never password values.** Whether a room
    /// wants a password is what an organizer needs to reconstruct why somebody
    /// could or could not get in; the secret itself has no business in a file
    /// that outlives the room and is handed to a person.
    pub fn options(at: f64, options: &crate::RoomOptions) -> Self {
        Self::new(
            "options",
            serde_json::json!({
                "at": at,
                "hint_cost": options.hint_cost,
                "location_check_points": options.location_check_points,
                "release_mode": options.release_mode.as_text(),
                "collect_mode": options.collect_mode.as_text(),
                "remaining_mode": options.remaining_mode.as_text(),
                "countdown_mode": options.countdown_mode.as_text(),
                "item_cheat": options.item_cheat,
                "compatibility": options.compatibility,
                "password_mode": match (&options.password, &options.slot_passwords) {
                    (_, Some(_)) => "per-slot",
                    (Some(_), None) => "room",
                    (None, None) => "none",
                },
                "server_password_set": options.server_password.is_some(),
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// One option changed on a live room, through `!admin /option`.
    pub fn option_changed(at: f64, option: &str, value: &str) -> Self {
        Self::new(
            "option_changed",
            serde_json::json!({ "at": at, "option": option, "value": value })
                .as_object()
                .expect("json! built an object")
                .clone(),
        )
    }

    /// A slot's password was set or cleared, with the value withheld.
    ///
    /// Clearing **locks** the slot rather than opening it, so `set: false` is
    /// the more consequential of the two and the reason this is recorded at
    /// all — "why can nobody join slot 4" is answerable from here.
    pub fn slot_password_changed(at: f64, slot: u32, set: bool) -> Self {
        Self::new(
            "slot_password_changed",
            serde_json::json!({ "at": at, "slot": slot, "set": set })
                .as_object()
                .expect("json! built an object")
                .clone(),
        )
    }

    /// Hints granted, with the point balance either side of the transaction.
    ///
    /// Both balances rather than the cost alone: hint price is a percentage of
    /// a slot's own location count and can be changed mid-room, so a cost in
    /// isolation cannot be checked against anything later. Before and after can.
    pub fn hints(
        at: f64,
        key: crate::SlotKey,
        player: &str,
        granted: Vec<String>,
        cost: i64,
        points_before: i64,
        points_after: i64,
    ) -> Self {
        Self::new(
            "hints",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "granted": granted,
                "cost": cost,
                "points_before": points_before,
                "points_after": points_after,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// An item conjured through the cheat console.
    ///
    /// The one item movement with no location behind it, and therefore the one
    /// the `check` records cannot account for. Without this the history reads
    /// as complete and quietly is not.
    pub fn cheat(at: f64, key: crate::SlotKey, player: &str, item: i64, item_name: &str) -> Self {
        Self::new(
            "cheat",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "item": item,
                "item_name": item_name,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// A `Bounce` carrying DeathLink.
    pub fn death_link(
        at: f64,
        key: crate::SlotKey,
        player: &str,
        cause: Option<&str>,
        source: Option<&str>,
        recipients: usize,
    ) -> Self {
        Self::new(
            "deathlink",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "cause": cause,
                "source": source,
                "recipients": recipients,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// One chat line, **as the room broadcast it**.
    ///
    /// Built from the same text that went to players, which for `!admin` is the
    /// masked form. Journalling anything earlier in that path would undo the
    /// masking into a file that outlives the room — the worst possible place
    /// for a password to reappear.
    pub fn chat(at: f64, key: crate::SlotKey, text: &str) -> Self {
        Self::new(
            "chat",
            serde_json::json!({ "at": at, "team": key.0, "slot": key.1, "text": text })
                .as_object()
                .expect("json! built an object")
                .clone(),
        )
    }
}

/// One location becoming checked, as the journal records it.
///
/// Every field is a number so the whole thing is `Copy` and costs an integer
/// copy to emit. `at` is the room's clock rather than a fresh `SystemTime`,
/// because the room has no clock of its own and reading one per record would be
/// a syscall inside the release loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckRecord {
    /// Unix seconds, from the room's clock.
    pub at: f64,
    /// The slot whose world contained the location.
    pub finder: u32,
    /// The slot the item belongs to.
    pub receiver: u32,
    pub item: i64,
    pub location: i64,
    pub flags: u32,
}

/// An [`EffectSink`] that records everything, for tests.
#[derive(Debug, Default)]
pub struct Recorder {
    pub events: Vec<Event>,
    pub dirty: bool,
    /// Checks the room offered to the journal, in the order it offered them.
    pub journal: Vec<CheckRecord>,
    /// Everything else the room offered the journal.
    pub journal_events: Vec<JournalEvent>,
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

    fn journal_check(&mut self, record: CheckRecord) {
        self.journal.push(record);
    }

    fn journal_event(&mut self, event: JournalEvent) {
        self.journal_events.push(event);
    }
}

impl Recorder {
    /// The journal events of one kind, for tests that care about a single sort.
    pub fn journal_events_of(&self, kind: &str) -> Vec<&JournalEvent> {
        self.journal_events
            .iter()
            .filter(|e| e.kind() == kind)
            .collect()
    }

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
        self.journal.clear();
        self.journal_events.clear();
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
