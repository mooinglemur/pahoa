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

    /// This connection's slot gained, lost or changed a send filter.
    ///
    /// A second copy of state the room owns, for the same reason
    /// [`EffectSink::membership_changed`] is: what a slot *receives* is decided
    /// where a broadcast's audience is expanded, which is the transport, not
    /// the room. Defaulted to nothing so the synchronous `Recorder` need not
    /// model it.
    fn filter_changed(
        &mut self,
        _conn: ConnId,
        _filter: Option<std::sync::Arc<crate::filter::Filter>>,
    ) {
    }

    /// One location was checked, for the room's durable history.
    ///
    /// An effect rather than something the room writes, for the usual reason —
    /// the room owns no files and has no clock beyond what it is handed. The
    /// transport decides whether anyone is listening, and a sink that is not
    /// journaling does nothing at all here.
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

    /// This build, beginning to serve the room.
    ///
    /// # The pair is what makes the history readable across restarts
    ///
    /// A journal spans every incarnation of a room by design — that is the
    /// whole reason it lives beside the save rather than in the log stream —
    /// but nothing in it said where one incarnation ended and the next began.
    /// A reader looking at a gap in the timestamps could not tell a quiet
    /// night from a crash, and could not tell which build produced the records
    /// on either side of it.
    ///
    /// **A [`started`](Self::started) with no [`stopped`](Self::stopped) before
    /// it is an unclean stop, and that is deliberate rather than a gap in the
    /// record.** A process killed outright — `SIGKILL`, an OOM kill, a node
    /// disappearing — writes nothing, because there is nothing that could
    /// write it. So the absence *is* the signal, and it is available to a
    /// reader who never saw the pod. The alternative would be a shutdown record
    /// written optimistically at start, which would say the opposite of the
    /// truth in exactly the case worth detecting.
    ///
    /// `build_rev` is the git revision, which is what makes "did this room's
    /// behavior change under it" answerable months later, when the version
    /// number alone has been reused by half a dozen builds.
    pub fn started(at: f64, version: &str, build_rev: &str) -> Self {
        Self::new(
            "started",
            serde_json::json!({ "at": at, "version": version, "build_rev": build_rev })
                .as_object()
                .expect("json! built an object")
                .clone(),
        )
    }

    /// This build, stopping cleanly, and what asked it to.
    ///
    /// `reason` is the same word the log line uses, so the two can be matched
    /// without a translation table: `SIGTERM` for an orchestrator draining the
    /// pod, `SIGINT` for a person at a terminal, `admin request` for
    /// `POST /admin/v1/shutdown`.
    ///
    /// The version is repeated here rather than left to the matching
    /// [`started`](Self::started) so that each record stands on its own — a
    /// reader tailing from a point in the middle sees a `stopped` whose build
    /// it never saw announced, and the same reasoning already makes `options`
    /// re-state itself on every start.
    pub fn stopped(at: f64, reason: &str, version: &str, build_rev: &str) -> Self {
        Self::new(
            "stopped",
            serde_json::json!({
                "at": at,
                "reason": reason,
                "version": version,
                "build_rev": build_rev,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// A connection that finished authenticating and joined a slot.
    ///
    /// **Only ever written after authentication succeeds**, which is what keeps
    /// this from being a way to write to somebody's disk: a port scan, a wrong
    /// password and a refused version all reach the room and produce nothing
    /// here. It is one record per *connection*, not per player — a slot running
    /// a game, a text client and a tracker joins three times, which is the
    /// thing an organizer is usually trying to account for.
    pub fn connected(
        at: f64,
        key: crate::SlotKey,
        player: &str,
        game: &str,
        version: &str,
        tags: &[String],
    ) -> Self {
        Self::new(
            "connected",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "game": game,
                "version": version,
                "tags": tags,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// A connection going away, and whether it was the slot's last one.
    ///
    /// `slot_empty` is the field worth having: a player closing one of their
    /// three clients is ordinary, and the slot going dark is the event somebody
    /// asks about later. Deriving it by replaying every `connected` and
    /// `disconnected` from the top of the file would work and is exactly the
    /// sort of bookkeeping a reader should not have to do.
    ///
    /// Pairs with [`connected`](Self::connected), so an unauthenticated
    /// connection that drops writes neither.
    pub fn disconnected(
        at: f64,
        key: crate::SlotKey,
        player: &str,
        tags: &[String],
        slot_empty: bool,
    ) -> Self {
        Self::new(
            "disconnected",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "tags": tags,
                "slot_empty": slot_empty,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// A connected client changing its tags.
    ///
    /// **Written only when the tags actually differ.** Trackers send
    /// `ConnectUpdate` routinely and most of those change nothing, so
    /// journaling the packet rather than the change would bury the file in
    /// records saying a client still wants what it already had.
    ///
    /// Worth recording because tags are not cosmetic: they decide whether a
    /// connection may claim the goal, whether it receives chat at all, and
    /// whether it counts as a game client — so a slot whose behavior changed
    /// mid-room changed it here.
    pub fn tags_changed(
        at: f64,
        key: crate::SlotKey,
        player: &str,
        from: &[String],
        to: &[String],
    ) -> Self {
        Self::new(
            "tags_changed",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "from": from,
                "to": to,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// A slot reaching its goal.
    ///
    /// The one status transition worth a line of its own: it is irreversible,
    /// it is what an organizer is asked to adjudicate, and it is what triggers
    /// auto-release and auto-collect — so the `check` records that follow are
    /// otherwise unexplained. The others (`Ready`, `Playing`) churn as clients
    /// come and go and say nothing durable.
    pub fn goal(at: f64, key: crate::SlotKey, player: &str, game: &str) -> Self {
        Self::new(
            "goal",
            serde_json::json!({
                "at": at,
                "team": key.0,
                "slot": key.1,
                "player": player,
                "game": game,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
    }

    /// A mutating command from the admin API, recorded where it was dispatched.
    ///
    /// # One record for sixteen commands, on purpose
    ///
    /// The admin surface wrote nothing at all before this, which left the
    /// journal describing a room that changed for no reason: an operator could
    /// conjure items, force hints, rename a slot, kick a player or release a
    /// world, and the history showed only the consequences. Worse, it was
    /// *inconsistent* — `!getitem` typed into chat is recorded as a `cheat`,
    /// while the same grant through `/send` was invisible, so the artifact used
    /// for adjudication depended on which door the operator came through.
    ///
    /// Recording the command at the single dispatch point rather than adding a
    /// bespoke record to each handler is what makes that hard to regress: a new
    /// admin verb is journaled because it is an admin verb, not because
    /// somebody remembered.
    ///
    /// **Nothing secret reaches `detail`, and that is enforced here rather than
    /// inherited.** The one command carrying a free value is `/option`. That
    /// path refuses every password-bearing option — but it refuses them in the
    /// *handler*, and this record is written before dispatch, so leaning on
    /// that refusal would write `server_password: topsecret` into a file that
    /// outlives the room every time an operator tried it and was told no. The
    /// value is masked on the option's name instead. Password *modes* travel in
    /// `options`, and slot passwords have their own boolean record.
    pub fn admin(at: f64, command: &str, slot: Option<u32>, detail: serde_json::Value) -> Self {
        Self::new(
            "admin",
            serde_json::json!({
                "at": at,
                "command": command,
                "slot": slot,
                "detail": detail,
            })
            .as_object()
            .expect("json! built an object")
            .clone(),
        )
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

    /// A `Bounce` carrying one of the cross-game *link* conventions.
    ///
    /// # DeathLink is not the only one
    ///
    /// The server relays all of them identically — they are ordinary `Bounce`
    /// traffic with a well-known tag — and upstream has three: `DeathLink`
    /// (implemented by 98 worlds), `TrapLink` (5) and `RingLink` (4). Recording
    /// only the first was a reasonable guess at what matters and an incomplete
    /// one: "why did I get a trap I never earned" is precisely the question an
    /// organizer gets asked, and it was the one thing the history could not
    /// answer.
    ///
    /// **What separates these from an arbitrary `Bounce` is volume, not
    /// importance.** A link fires on a discrete game event, so its rate is
    /// bounded by play. A fork's or a tracker's own relay traffic is bounded by
    /// nothing, and journaling all of it would let one chatty client dominate
    /// a file somebody else has to read.
    ///
    /// `kind` is the record type, so a reader dispatches on `deathlink`,
    /// `traplink` or `ringlink`, and `extra` carries the convention's own
    /// payload — `cause`, `trap_name`, `amount` — beside the fields they share.
    /// It is merged at the top level rather than nested so that the `deathlink`
    /// record keeps the exact shape it already had.
    ///
    /// # `source` is the client's claim; `slot` is the fact
    ///
    /// **These can disagree and the record deliberately carries both.** `source`
    /// is copied out of the bounce payload, so it is whatever the sending
    /// client chose to put there — unvalidated, and nothing stops a client
    /// naming somebody else. `team`, `slot` and `player` come from the
    /// authenticated connection the packet arrived on, so they are the room's
    /// own answer to who sent it and cannot be spoofed.
    ///
    /// A history that recorded only the claim would be unusable for the one
    /// thing it is for: an organizer asked "who killed me" needs the answer the
    /// server knows, not the one the packet asserted.
    ///
    /// `source` is absent for `RingLink`, and honestly so: that convention puts
    /// a client instance id there rather than a player name, so there is no
    /// name to record — another reason not to lean on it.
    pub fn link(
        at: f64,
        kind: &str,
        key: crate::SlotKey,
        player: &str,
        source: Option<&str>,
        recipients: usize,
        extra: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let mut fields = serde_json::json!({
            "at": at,
            "team": key.0,
            "slot": key.1,
            "player": player,
            "source": source,
            "recipients": recipients,
        })
        .as_object()
        .expect("json! built an object")
        .clone();
        fields.extend(extra);
        Self::new(kind, fields)
    }

    /// One chat line, **as the room broadcast it**.
    ///
    /// Built from the same text that went to players, which for `!admin` is the
    /// masked form. Journaling anything earlier in that path would undo the
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
