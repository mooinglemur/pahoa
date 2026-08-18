//! The room state machine.
//!
//! Owns everything mutable about a live multiworld and produces outbound
//! packets through an [`EffectSink`]. No sockets, no clock beyond what callers
//! pass in, no async.

mod commands;

use crate::conn::{Client, ConnId, non_game_verb, python_list_repr};
use crate::datapackage::DataPackageCache;
use crate::effect::{CloseReason, EffectSink, Recipients};
use crate::hints::HintStore;
use crate::options::RoomOptions;
use crate::save::{SaveError, Snapshot};
use pahoa_multidata::{DataPackage as NameTables, Hint, MultiData, SlotType};
use pahoa_proto::server::*;
use pahoa_proto::types::*;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_pyrandom::PyRandom;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Server version reported in `RoomInfo`. Tracks the Archipelago release whose
/// behavior this implementation reproduces.
pub const SERVER_VERSION: Version = Version::new(0, 6, 8);

/// `PrintJSON` packets per broadcast frame.
///
/// Chosen by Archipelago to sit near the compression window without producing
/// oversized frames (`MultiServer.py:1165-1167`); reproducing it keeps
/// bandwidth behavior and frame counts comparable.
const PRINT_JSON_CHUNK: usize = 140;

/// `(team, slot)`. Teams beyond 0 never occur today — the reference server only
/// ever creates team 0 — but the key shape is everywhere in the protocol and
/// retrofitting it later would touch every structure, so it is kept from the start.
pub type SlotKey = (u32, u32);

/// `(team, slot, remote_items)` — the key of one of a slot's two item queues.
///
/// Two, not one, because a client that handles its own world's items locally
/// gets a different stream from one that wants everything back from the server
/// (`MultiServer.py:1126-1131`).
pub type QueueKey = (u32, u32, bool);

/// A running `!countdown`.
///
/// The reference spawns a task that sleeps a second per step
/// (`MultiServer.py:1846-1858`). Holding the state here instead keeps the room
/// clockless: it says *when* it next wants to be poked, and the transport does
/// the poking.
#[derive(Debug, Clone, Copy)]
struct Countdown {
    /// The number the next tick announces. Zero means the next tick says "GO".
    remaining: i64,
    /// Absolute time of that tick, on the same scale as [`Room::start_time`].
    next_at: f64,
}

pub struct Room {
    data: Arc<MultiData>,
    datapackage: Arc<NameTables>,
    /// The `GetDataPackage` reply, rendered once at construction.
    ///
    /// See [`DataPackageCache`] for why this is not built per request.
    served_datapackage: DataPackageCache,
    pub options: RoomOptions,

    clients: HashMap<ConnId, Client>,
    /// Connections currently authenticated to each slot. Co-op means this is a
    /// list, not a single entry.
    by_slot: HashMap<SlotKey, Vec<ConnId>>,

    /// Behind an `Arc` so [`Room::snapshot`] is a refcount bump per slot rather
    /// than a deep clone. At 2000 slots and 400k checks a deep clone is tens of
    /// milliseconds of mailbox stall on every save, however fast the disk is.
    /// Mutated through [`Arc::make_mut`]: with no save in flight the refcount is
    /// 1 and that is free, and with one in flight only the slots actually
    /// touched are cloned.
    location_checks: HashMap<SlotKey, Arc<HashSet<i64>>>,
    /// Two queues per slot, keyed on the connection's `remote_items` setting:
    /// a client that does not want its own world's items gets a different
    /// stream from one that does (`MultiServer.py:1126-1131`).
    ///
    /// `Arc` for the same reason as `location_checks`.
    received_items: HashMap<QueueKey, Arc<Vec<NetworkItem>>>,
    client_game_state: HashMap<SlotKey, ClientStatus>,
    name_aliases: HashMap<SlotKey, String>,
    hints_used: HashMap<SlotKey, i64>,
    hints: HintStore,
    /// Seeded from the seed name, exactly as the reference does at load
    /// (`MultiServer.py:535`), so hint ordering is reproducible for a seed.
    /// Its state belongs in the save.
    ///
    /// Seeded here rather than where it is first drawn from, because the draw
    /// sequence has to start from the same place the reference's does. Nothing
    /// consumes it until `!hint` picks between candidate hints.
    #[allow(dead_code)]
    rng: PyRandom,

    /// Slots granted a one-off release by an administrator, over and above
    /// what `release_mode` allows.
    allow_releases: HashSet<SlotKey>,
    /// Which members of each item-link group have collected, so the group's own
    /// slot collects once they all have (`MultiServer.py:1113-1118`).
    group_collected: HashMap<u32, HashSet<u32>>,
    /// The running `!countdown`, if any.
    countdown: Option<Countdown>,

    /// Free-form client key-value store.
    ///
    /// `Arc` per value for the snapshot's sake, as above. No `make_mut` is ever
    /// needed here: `Set` computes a fresh value and replaces the entry, so a
    /// snapshot's `Arc` is never the one being written through.
    stored_data: HashMap<String, Arc<Value>>,
    /// Who to notify when a key changes.
    stored_data_subscriptions: HashMap<String, HashSet<ConnId>>,

    /// Server start time, reported in `RoomInfo.time` for DeathLink sync.
    pub start_time: f64,
    /// The last time the transport reported through [`Room::tick`].
    ///
    /// The room has no clock of its own — that is what lets a 400k-location
    /// release run in a synchronous test — so anything time-dependent reads
    /// this instead. It only has to be roughly current, and the transport
    /// refreshes it on every batch it processes.
    clock: f64,
}

impl Room {
    pub fn new(
        data: Arc<MultiData>,
        datapackage: Arc<NameTables>,
        options: RoomOptions,
        start_time: f64,
    ) -> Self {
        let mut client_game_state = HashMap::new();
        // Slots that are not exactly `player` — spectators and item-link groups
        // — count as finished the moment the room loads, so they never block a
        // team-completion check (`MultiServer.py:551-555`).
        for (slot, info) in &data.slot_info {
            if info.slot_type.always_goal() {
                client_game_state.insert((0, *slot), ClientStatus::Goal);
            }
        }

        // Hints baked into the seed — placed by the generator, not bought.
        let mut hints = HintStore::default();
        for (slot, seeded) in &data.precollected_hints {
            for hint in seeded {
                hints.upsert((0, *slot), hint.clone());
            }
        }

        let rng = PyRandom::seed_str(&data.seed_name);
        let served_datapackage = DataPackageCache::build(&datapackage);

        Self {
            data,
            datapackage,
            served_datapackage,
            options,
            clients: HashMap::new(),
            by_slot: HashMap::new(),
            location_checks: HashMap::new(),
            received_items: HashMap::new(),
            client_game_state,
            name_aliases: HashMap::new(),
            hints_used: HashMap::new(),
            hints,
            rng,
            allow_releases: HashSet::new(),
            group_collected: HashMap::new(),
            countdown: None,
            stored_data: HashMap::new(),
            stored_data_subscriptions: HashMap::new(),
            start_time,
            clock: start_time,
        }
    }

    pub fn multidata(&self) -> &MultiData {
        &self.data
    }

    /// The merged name tables. Immutable and shared, so callers off the actor
    /// can hold it.
    pub fn datapackage(&self) -> &Arc<NameTables> {
        &self.datapackage
    }

    pub fn client(&self, conn: ConnId) -> Option<&Client> {
        self.clients.get(&conn)
    }

    // --- lifecycle -------------------------------------------------------

    /// A socket connected. Archipelago sends `RoomInfo` immediately, before any
    /// authentication (`MultiServer.py:921-940`).
    pub fn on_connect(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        self.clients.insert(conn, Client::new(conn));
        out.send(conn, &[ServerPacket::RoomInfo(self.room_info())]);
    }

    pub fn on_disconnect(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let Some(client) = self.clients.remove(&conn) else {
            return;
        };
        if !client.auth {
            return;
        }

        let key = (client.team, client.slot);
        if let Some(conns) = self.by_slot.get_mut(&key) {
            conns.retain(|c| *c != conn);
        }
        // Python relies on a WeakSet and garbage collection for this; pruning
        // explicitly is the same effect without the GC timing dependency.
        for subscribers in self.stored_data_subscriptions.values_mut() {
            subscribers.remove(&conn);
        }

        // Only the *status reset* waits for the slot to be empty
        // (`MultiServer.py:990-993`). The announcement does not: the reference
        // broadcasts it for every departing connection, outside that guard.
        // Having it inside meant a slot with a game and a tracker attached said
        // nothing at all when one of them went away.
        if self.by_slot.get(&key).is_none_or(Vec::is_empty) {
            self.client_game_state
                .entry(key)
                .and_modify(|s| {
                    if *s != ClientStatus::Goal {
                        *s = ClientStatus::Unknown;
                    }
                })
                .or_insert(ClientStatus::Unknown);
        }

        // `MultiServer.py:1001-1006`. A non-game client "stopped tracking"
        // rather than "left", so the verb is a phrase and not a parenthetical.
        let verb = match non_game_verb(&client.tags) {
            Some(v) => format!("stopped {v}"),
            None => "left".to_string(),
        };
        let text = format!(
            "{} (Team #{}) has {verb} the game. Client({}), {}.",
            self.slot_alias(key),
            client.team + 1,
            client.version,
            python_list_repr(&client.tags),
        );
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Part),
                team: Some(client.team),
                slot: Some(client.slot),
                ..Default::default()
            })],
        );
    }

    // --- packet dispatch -------------------------------------------------

    pub fn handle(&mut self, conn: ConnId, packet: ClientPacket, out: &mut dyn EffectSink) {
        let authed = self.clients.get(&conn).is_some_and(|c| c.auth);

        // Before authentication only Connect and GetDataPackage are processed.
        // Everything else falls through Python's `elif client.auth:` chain and
        // is silently ignored — not refused (`MultiServer.py:1963`).
        if !authed && !packet.allowed_before_auth() {
            return;
        }

        match packet {
            ClientPacket::Connect(c) => self.handle_connect(conn, *c, out),
            ClientPacket::GetDataPackage(g) => self.handle_get_datapackage(conn, g, out),
            ClientPacket::Sync => self.handle_sync(conn, out),
            ClientPacket::LocationChecks(l) => self.handle_location_checks(conn, l, out),
            ClientPacket::StatusUpdate(s) => self.handle_status_update(conn, s, out),
            ClientPacket::ConnectUpdate(u) => self.handle_connect_update(conn, u, out),
            ClientPacket::Get(g, raw) => self.handle_get(conn, g, raw, out),
            ClientPacket::Set(s, raw) => self.handle_set(conn, *s, raw, out),
            ClientPacket::SetNotify(s) => self.handle_set_notify(conn, s),
            ClientPacket::Bounce(b, raw) => self.handle_bounce(conn, b, raw, out),
            ClientPacket::LocationScouts(s) => self.handle_location_scouts(conn, s, out),
            ClientPacket::CreateHints(c) => self.handle_create_hints(conn, c, out),
            ClientPacket::UpdateHint(u) => self.handle_update_hint(conn, u, out),
            ClientPacket::Say(s) => self.handle_say(conn, s, out),
        }
    }

    /// Refuse a command's arguments without dropping the connection.
    fn bad_arguments(&self, conn: ConnId, cmd: &str, text: String, out: &mut dyn EffectSink) {
        out.send(
            conn,
            &[ServerPacket::InvalidPacket(InvalidPacket {
                problem_type: "arguments".into(),
                original_cmd: Some(cmd.to_string()),
                text,
            })],
        );
    }

    /// Drop the connection, reproducing a handler that raises in the reference.
    ///
    /// `process_client_cmd` is called inside the socket's read loop with no
    /// per-command guard, so an exception unwinds to `server()`'s handler and
    /// the client is disconnected (`MultiServer.py:900-917`).
    fn protocol_error(&self, conn: ConnId, text: String, out: &mut dyn EffectSink) {
        out.close(conn, CloseReason::ProtocolError(text));
    }

    // --- Connect ---------------------------------------------------------

    fn handle_connect(&mut self, conn: ConnId, args: cmd::Connect, out: &mut dyn EffectSink) {
        let mut errors: Vec<ConnectionRefusedReason> = Vec::new();

        // The room-wide password can be checked before anything about the
        // client is known. A per-slot one cannot, so it waits until the name has
        // resolved, below.
        if !crate::secret::ct_eq_opt(self.options.password.as_deref(), args.password.as_deref()) {
            errors.push(ConnectionRefusedReason::InvalidPassword);
        }

        let resolved = self.data.connect_names.get(&args.name).copied();
        let mut items_handling = ItemsHandling::new(0).expect("0 is valid");

        match resolved {
            None => errors.push(ConnectionRefusedReason::InvalidSlot),
            Some((_team, slot)) => {
                // Now that the slot is known, its own password applies. A slot
                // absent from the map has none. Pushed as the same
                // `InvalidPassword` the room-wide check uses, so which of the
                // two modes is in force is not something a caller can probe.
                if !crate::secret::ct_eq_opt(
                    self.options.slot_passwords.get(&slot).map(String::as_str),
                    args.password.as_deref(),
                ) {
                    errors.push(ConnectionRefusedReason::InvalidPassword);
                }

                let ignore_game = Client::ignores_game(&args.game, &args.tags);
                let expected_game = self.slot_game(slot);

                if !ignore_game && args.game.as_deref() != Some(expected_game.as_str()) {
                    errors.push(ConnectionRefusedReason::InvalidGame);
                }

                // A game-less tracker is held only to the global floor; a real
                // player is held to its slot's floor (`MultiServer.py:1888-1890`).
                let minver = if ignore_game {
                    Version::from(pahoa_multidata::MIN_CLIENT_VERSION)
                } else {
                    Version::from(self.data.min_client_version(slot))
                };
                if minver > args.version {
                    errors.push(ConnectionRefusedReason::IncompatibleVersion);
                }

                match ItemsHandling::new(args.items_handling) {
                    Ok(h) => items_handling = h,
                    Err(_) => errors.push(ConnectionRefusedReason::InvalidItemsHandling),
                }
            }
        }

        // Tournament mode demands an exact match, not merely a new-enough client.
        if self.options.compatibility == 0 && args.version != SERVER_VERSION {
            errors.push(ConnectionRefusedReason::IncompatibleVersion);
        }

        if !errors.is_empty() {
            // Python accumulates into a set, so its order is arbitrary; ours is
            // deterministic, and duplicates are removed to match set semantics.
            errors.dedup();
            out.send(
                conn,
                &[ServerPacket::ConnectionRefused(ConnectionRefused {
                    errors,
                })],
            );
            return;
        }

        let (team, slot) = resolved.expect("no errors means the slot resolved");
        let key = (team, slot);

        let was_authed = {
            let client = self
                .clients
                .get_mut(&conn)
                .expect("connection is registered");
            let previously = client.auth;
            let moved = previously && (client.team != team || client.slot != slot);

            if previously {
                let old = (client.team, client.slot);
                if let Some(conns) = self.by_slot.get_mut(&old) {
                    conns.retain(|c| *c != conn);
                }
            }

            client.team = team;
            client.slot = slot;
            client.version = args.version;
            client.items_handling = items_handling;
            client.apply_tags(args.tags.clone());
            // Swapping slot re-announces the join; reconnecting to the same slot
            // does not (`MultiServer.py:1906-1909`).
            if moved {
                client.auth = false;
            }
            client.auth
        };

        self.by_slot.entry(key).or_default().push(conn);
        self.client_game_state
            .entry(key)
            .or_insert(ClientStatus::Connected);

        let mut reply = Vec::with_capacity(2);

        let slot_data = if args.slot_data {
            self.slot_data_json(slot)
        } else {
            None
        };

        reply.push(ServerPacket::Connected(Box::new(Connected {
            team,
            slot,
            players: self.players_package(),
            missing_locations: self.missing_locations(key),
            checked_locations: self.checked_locations(key),
            slot_info: self.slot_info_package(),
            hint_points: self.slot_points(key),
            slot_data,
        })));

        let client = &self.clients[&conn];
        if !client.items_handling.no_items() {
            let items = self.items_for(conn);
            if !items.is_empty() {
                let len = items.len();
                reply.push(ServerPacket::ReceivedItems(ReceivedItems {
                    index: 0,
                    items,
                }));
                self.clients.get_mut(&conn).expect("registered").send_index = len;
            }
        }

        if !was_authed {
            self.clients.get_mut(&conn).expect("registered").auth = true;
            // Before the announcement, not after: the transport filters
            // broadcasts on its own copy of `auth`, so a late update leaves the
            // joining client out of its own join message.
            let client = &self.clients[&conn];
            out.membership_changed(conn, true, client.no_text, Some((client.team, client.slot)));
            self.announce_join(conn, out);
        }

        out.send(conn, &reply);
    }

    fn announce_join(&self, conn: ConnId, out: &mut dyn EffectSink) {
        let client = &self.clients[&conn];
        let key = (client.team, client.slot);
        let verb = non_game_verb(&client.tags).unwrap_or("playing");
        // `MultiServer.py:972-976`, verbatim. Every part of the shape matters
        // because players read it: the parenthesized field is the *team*, the
        // verb precedes the game, and the trailing field is the tag list.
        let text = format!(
            "{} (Team #{}) {verb} {} has joined. Client({}), {}.",
            self.slot_alias(key),
            client.team + 1,
            self.slot_game(client.slot),
            client.version,
            python_list_repr(&client.tags),
        );
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Join),
                team: Some(client.team),
                slot: Some(client.slot),
                tags: Some(client.tags.clone()),
                ..Default::default()
            })],
        );

        // The reference follows every join with this, privately
        // (`MultiServer.py:977-982`). It is how a player discovers `!help`
        // exists at all, so its absence is a functional gap, not a cosmetic one.
        out.send(
            conn,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(
                    "Now that you are connected, you can use !help to list commands \
                     to run via the server. If your client supports it, you may have \
                     additional local commands you can list with /help."
                        .to_string(),
                )],
                print_type: Some(PrintJsonType::Tutorial),
                ..Default::default()
            })],
        );
    }

    fn handle_connect_update(
        &mut self,
        conn: ConnId,
        args: cmd::ConnectUpdate,
        out: &mut dyn EffectSink,
    ) {
        let mut resend = false;
        {
            let Some(client) = self.clients.get_mut(&conn) else {
                return;
            };
            if let Some(bits) = args.items_handling {
                match ItemsHandling::new(bits) {
                    Ok(h) => {
                        if h != client.items_handling {
                            client.items_handling = h;
                            resend = true;
                        }
                    }
                    Err(_) => {
                        out.send(
                            conn,
                            &[ServerPacket::InvalidPacket(InvalidPacket {
                                problem_type: "arguments".into(),
                                original_cmd: Some("ConnectUpdate".into()),
                                text: "Invalid items_handling flag combination".into(),
                            })],
                        );
                        return;
                    }
                }
            }
            if let Some(tags) = args.tags {
                client.apply_tags(tags);
            }
        }

        // Changing items_handling restarts the item stream from zero, because
        // the client is now asking for a different set (`MultiServer.py:1975-1978`).
        if resend {
            self.resend_all_items(conn, out);
        }
    }

    fn handle_sync(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        self.resend_all_items(conn, out);
    }

    fn resend_all_items(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        if client.items_handling.no_items() {
            return;
        }
        let items = self.items_for(conn);
        let len = items.len();
        out.send(
            conn,
            &[ServerPacket::ReceivedItems(ReceivedItems {
                index: 0,
                items,
            })],
        );
        if let Some(c) = self.clients.get_mut(&conn) {
            c.send_index = len;
        }
    }

    fn handle_get_datapackage(
        &mut self,
        conn: ConnId,
        args: cmd::GetDataPackage,
        out: &mut dyn EffectSink,
    ) {
        let wanted: Vec<&str> = match (&args.games, &args.exclusions) {
            (Some(games), _) => games.iter().map(String::as_str).collect(),
            // Deprecated, past its own removal TODO, still honored.
            (None, Some(excluded)) => self
                .datapackage
                .games()
                .map(|(g, _)| g.as_str())
                .filter(|g| !excluded.iter().any(|e| e == g))
                .collect(),
            (None, None) => self.datapackage.games().map(|(g, _)| g.as_str()).collect(),
        };

        out.send(
            conn,
            &[ServerPacket::DataPackage(DataPackage {
                data: self.served_datapackage.select(&wanted),
            })],
        );
    }

    // --- data storage ----------------------------------------------------

    /// Keys beginning `_read_` are computed views of room state rather than
    /// stored values, and cannot be written (`MultiServer.py:498-577`).
    const READ_PREFIX: &'static str = "_read_";

    /// Resolve a `_read_*` key. `None` means the key names nothing.
    fn read_data(&self, key: &str) -> Option<Value> {
        let name = key.strip_prefix(Self::READ_PREFIX)?;

        if name == "race_mode" {
            return Some(Value::from(u8::from(self.data.race_mode)));
        }
        if let Some(rest) = name.strip_prefix("slot_data_") {
            let slot: u32 = rest.parse().ok()?;
            return Some(match self.slot_data_json(slot) {
                Some(raw) => serde_json::from_str(raw.get()).unwrap_or(Value::Null),
                None => Value::Null,
            });
        }
        if let Some(rest) = name.strip_prefix("client_status_") {
            let (team, slot) = parse_team_slot(rest)?;
            return Some(Value::from(self.status((team, slot)) as u8));
        }
        if let Some(rest) = name.strip_prefix("item_name_groups_") {
            return Some(groups_to_json(
                self.datapackage
                    .get(rest)
                    .map(|n| &n.package.item_name_groups),
            ));
        }
        if let Some(rest) = name.strip_prefix("location_name_groups_") {
            return Some(groups_to_json(
                self.datapackage
                    .get(rest)
                    .map(|n| &n.package.location_name_groups),
            ));
        }
        if let Some(rest) = name.strip_prefix("hints_") {
            let key = parse_team_slot(rest)?;
            return Some(self.hints_json(key));
        }
        None
    }

    /// `Get`: the reply is the request map with `cmd` rewritten and `keys`
    /// replaced, so any extra fields the client attached ride along
    /// (`MultiServer.py:2162-2174`).
    fn handle_get(
        &mut self,
        conn: ConnId,
        args: cmd::Get,
        raw: Map<String, Value>,
        out: &mut dyn EffectSink,
    ) {
        let mut values = Map::new();
        for key in &args.keys {
            let value = if key.starts_with(Self::READ_PREFIX) {
                self.read_data(key).unwrap_or(Value::Null)
            } else {
                self.stored_data
                    .get(key)
                    .map_or(Value::Null, |v| (**v).clone())
            };
            values.insert(key.clone(), value);
        }
        out.send(
            conn,
            &[ServerPacket::echo(
                raw,
                "Retrieved",
                &[("keys", Value::Object(values))],
            )],
        );
    }

    fn handle_set(
        &mut self,
        conn: ConnId,
        args: cmd::Set,
        raw: Map<String, Value>,
        out: &mut dyn EffectSink,
    ) {
        if args.key.starts_with(Self::READ_PREFIX) {
            out.send(
                conn,
                &[ServerPacket::InvalidPacket(InvalidPacket {
                    problem_type: "arguments".into(),
                    original_cmd: Some("Set".into()),
                    text: format!("cannot write to the read-only key {:?}", args.key),
                })],
            );
            return;
        }

        // An absent key falls back to the packet's `default`, or 0 — not null
        // (`MultiServer.py:2183`).
        let original = self.stored_data.get(&args.key).map_or_else(
            || args.default.clone().unwrap_or(Value::from(0)),
            |v| (**v).clone(),
        );

        let operations: Vec<(String, Value)> = args
            .operations
            .iter()
            .map(|o| (o.operation.clone(), o.value.clone()))
            .collect();

        let value = match pahoa_datastore::apply_all(original.clone(), &operations) {
            Ok(v) => v,
            Err((index, e)) => {
                // The reference raises here and drops the socket rather than
                // answering; reproduce that, but say why in the log.
                let slot = self.clients.get(&conn).map(|c| c.slot).unwrap_or(0);
                out.close(
                    conn,
                    CloseReason::ProtocolError(format!(
                        "slot {slot}: Set operation {index} ({}) failed: {e}",
                        operations.get(index).map(|o| o.0.as_str()).unwrap_or("?"),
                    )),
                );
                return;
            }
        };

        self.stored_data
            .insert(args.key.clone(), Arc::new(value.clone()));
        out.mark_dirty();

        let slot = self.clients.get(&conn).map(|c| c.slot).unwrap_or(0);
        let reply = ServerPacket::echo(
            raw,
            "SetReply",
            &[
                ("original_value", original),
                ("value", value),
                ("slot", Value::from(slot)),
            ],
        );

        // The setter only hears back if it asked; subscribers always do, and
        // a SetReply is sent even when the value did not change.
        let mut targets: Vec<ConnId> = self
            .stored_data_subscriptions
            .get(&args.key)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        if args.want_reply && !targets.contains(&conn) {
            targets.push(conn);
        }
        if !targets.is_empty() {
            targets.sort_unstable();
            out.broadcast(Recipients::These(targets), &[reply]);
        }
    }

    /// Subscriptions are never explicitly removed — Python uses a `WeakSet` and
    /// lets garbage collection do it. Holding connection ids and pruning on
    /// disconnect is the same behavior without the GC timing dependency.
    fn handle_set_notify(&mut self, conn: ConnId, args: cmd::SetNotify) {
        for key in args.keys {
            self.stored_data_subscriptions
                .entry(key)
                .or_default()
                .insert(conn);
        }
    }

    // --- bounce ----------------------------------------------------------

    /// Forward a `Bounce` to everyone matching **any** of its filters, on the
    /// same team, including the sender (`MultiServer.py:2149-2160`).
    ///
    /// This is what carries DeathLink.
    fn handle_bounce(
        &mut self,
        conn: ConnId,
        args: cmd::Bounce,
        raw: Map<String, Value>,
        out: &mut dyn EffectSink,
    ) {
        let Some(sender) = self.clients.get(&conn) else {
            return;
        };
        let team = sender.team;

        let games = args.games.unwrap_or_default();
        let slots = args.slots.unwrap_or_default();
        let tags = args.tags.unwrap_or_default();
        // No filters at all matches nobody, which is what an empty `any()` does.
        let targets: Vec<ConnId> = self
            .clients
            .values()
            .filter(|c| c.auth && c.team == team)
            .filter(|c| {
                slots.contains(&c.slot)
                    || tags.iter().any(|t| c.tags.contains(t))
                    || games.contains(&self.slot_game(c.slot))
            })
            .map(|c| c.id)
            .collect();

        if targets.is_empty() {
            return;
        }
        let mut targets = targets;
        targets.sort_unstable();
        out.broadcast(
            Recipients::These(targets),
            &[ServerPacket::echo(raw, "Bounced", &[])],
        );
    }

    // --- status ----------------------------------------------------------

    fn handle_status_update(
        &mut self,
        conn: ConnId,
        args: cmd::StatusUpdate,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let (team, slot) = (client.team, client.slot);

        // Trackers and text clients may not claim the goal.
        if client.no_locations && args.status == ClientStatus::Goal as i64 {
            out.send(
                conn,
                &[ServerPacket::InvalidPacket(InvalidPacket {
                    problem_type: "arguments".into(),
                    original_cmd: Some("StatusUpdate".into()),
                    text: "Tracker and TextOnly clients cannot report goal completion".into(),
                })],
            );
            return;
        }

        let Ok(status) = ClientStatus::from_i64(args.status, &pahoa_multidata::Path::root()) else {
            out.send(
                conn,
                &[ServerPacket::InvalidPacket(InvalidPacket {
                    problem_type: "arguments".into(),
                    original_cmd: Some("StatusUpdate".into()),
                    text: format!("Unknown client status {}", args.status),
                })],
            );
            return;
        };

        self.set_status((team, slot), status, out);
    }

    /// Goal is irreversible once reached (`MultiServer.py:2206-2214`).
    fn set_status(&mut self, key: SlotKey, status: ClientStatus, out: &mut dyn EffectSink) {
        let current = self.status(key);
        if current == ClientStatus::Goal {
            return;
        }
        self.client_game_state.insert(key, status);
        out.mark_dirty();

        if status == ClientStatus::Goal {
            self.on_goal_achieved(key, out);
        }
    }

    /// `on_goal_achieved` (`MultiServer.py:857-866`).
    ///
    /// Collect runs before release, which matters: collecting first means the
    /// finished player's own inventory is settled before their world is
    /// emptied out to everyone else.
    fn on_goal_achieved(&mut self, key: SlotKey, out: &mut dyn EffectSink) {
        let text = format!(
            "{} (Team #{}) has completed their goal.",
            self.slot_alias(key),
            key.0 + 1
        );
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Goal),
                team: Some(key.0),
                slot: Some(key.1),
                ..Default::default()
            })],
        );

        if self.options.collect_mode.is_auto() {
            self.collect_player(key, out);
        }
        if self.options.release_mode.is_auto() {
            self.release_player(key, out);
        }
    }

    pub fn status(&self, key: SlotKey) -> ClientStatus {
        self.client_game_state
            .get(&key)
            .copied()
            .unwrap_or(ClientStatus::Unknown)
    }

    // --- location checks -------------------------------------------------

    fn handle_location_checks(
        &mut self,
        conn: ConnId,
        args: cmd::LocationChecks,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        if client.no_locations {
            out.send(
                conn,
                &[ServerPacket::InvalidPacket(InvalidPacket {
                    problem_type: "arguments".into(),
                    original_cmd: Some("LocationChecks".into()),
                    text: "Tracker and TextOnly clients cannot check locations".into(),
                })],
            );
            return;
        }
        let key = (client.team, client.slot);
        self.register_location_checks(key, &args.locations, out);
    }

    /// The hot path.
    ///
    /// Diffs against what the slot has already checked, drops ids the multidata
    /// does not know, distributes each item to its receiver, and broadcasts the
    /// item feed in chunks. Two things differ from the reference on purpose:
    ///
    /// - only slots that actually received something are swept when sending new
    ///   items, instead of every connected client (`MultiServer.py:1070-1084`
    ///   iterates the lot on every batch, which is O(clients) per check and
    ///   untenable at 6000 connections)
    /// - the sort key is the same `(receiver, item, location, flags)` tuple, so
    ///   the resulting order is identical
    pub fn register_location_checks(
        &mut self,
        key: SlotKey,
        locations: &[i64],
        out: &mut dyn EffectSink,
    ) {
        let (team, slot) = key;
        let already = self.location_checks.entry(key).or_default();

        // Unknown ids are dropped silently: clients legitimately send ids for
        // locations this multidata does not contain.
        let mut fresh: Vec<i64> = Vec::new();
        for &loc in locations {
            if !already.contains(&loc) && self.data.locations.contains(slot, loc) {
                fresh.push(loc);
            }
        }
        if fresh.is_empty() {
            return;
        }
        fresh.sort_unstable();
        fresh.dedup();

        let mut sortable: Vec<(u32, i64, i64, u32)> = Vec::with_capacity(fresh.len());
        for &loc in &fresh {
            let e = self.data.locations.get(slot, loc).expect("checked above");
            sortable.push((e.receiver, e.item, e.location, e.flags));
        }
        // Group by receiver then item, matching `MultiServer.py:1143-1148`; the
        // resulting item order is visible to clients.
        sortable.sort_unstable();

        let mut dirty_slots: HashSet<u32> = HashSet::new();
        let mut feed: Vec<ServerPacket> = Vec::with_capacity(PRINT_JSON_CHUNK);

        for (receiver, item, location, flags) in sortable {
            let net = NetworkItem {
                item,
                location,
                player: slot,
                flags,
            };
            self.send_item_to(team, receiver, net);
            dirty_slots.insert(receiver);
            for member in self.group_members_of(receiver) {
                dirty_slots.insert(member);
            }

            if feed.len() >= PRINT_JSON_CHUNK {
                out.broadcast(Recipients::AllText, &feed);
                feed.clear();
            }
            feed.push(Self::item_send_message(receiver, net));
        }
        if !feed.is_empty() {
            out.broadcast(Recipients::AllText, &feed);
        }

        Arc::make_mut(self.location_checks.entry(key).or_default()).extend(fresh.iter().copied());

        self.send_new_items(&dirty_slots, out);

        // Only the *new* checks go out here; the full list is sent by a separate
        // path. Same field name, two meanings — clients union rather than replace.
        if self.by_slot.get(&key).is_some_and(|c| !c.is_empty()) {
            out.broadcast(
                Recipients::Slot(key),
                &[ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                    checked_locations: Some(fresh),
                    hint_points: Some(self.slot_points(key)),
                    ..Default::default()
                }))],
            );
        }

        // Hints on the locations just checked become "found". Only hints this
        // slot *finds* can change, so the sweep is bounded by that.
        for changed in self.recheck_hints(slot) {
            self.on_changed_hints(changed, out);
        }

        out.mark_dirty();
    }

    /// Refresh the found flag on every hint `finder` is responsible for, and
    /// report whose hint lists changed.
    ///
    /// The reference does this lazily too, on every read of `_read_hints_*`
    /// (`MultiServer.py:758-760`). Doing it eagerly here instead is equivalent —
    /// registering checks is the only thing that can make a hint found — and it
    /// keeps a tracker polling that key off an O(all hints) path.
    fn recheck_hints(&mut self, finder: u32) -> Vec<SlotKey> {
        let Self {
            hints,
            location_checks,
            ..
        } = self;
        // Team is 0 throughout; the reference indexes `location_checks` by the
        // team of the hint list being rechecked, which is the same thing today.
        hints.recheck(finder, &|slot, location| {
            location_checks
                .get(&(0, slot))
                .is_some_and(|c| c.contains(&location))
        })
    }

    /// The full `checked_locations` list, as against the incremental one
    /// `register_location_checks` sends.
    ///
    /// Same field name, two meanings — clients union rather than replace, which
    /// is what makes both correct (`MultiServer.py:1130-1132`).
    fn update_checked_locations(&self, key: SlotKey, out: &mut dyn EffectSink) {
        out.broadcast(
            Recipients::Slot(key),
            &[ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                checked_locations: Some(self.checked_locations(key)),
                ..Default::default()
            }))],
        );
    }

    /// `release_player` (`MultiServer.py:1091-1098`): give up on the rest of a
    /// world and send everything still in it.
    pub fn release_player(&mut self, key: SlotKey, out: &mut dyn EffectSink) {
        let text = format!(
            "{} (Team #{}) has released all remaining items from their world.",
            self.slot_name(key),
            key.0 + 1
        );
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Release),
                team: Some(key.0),
                slot: Some(key.1),
                ..Default::default()
            })],
        );

        let all: Vec<i64> = self
            .data
            .locations
            .for_slot(key.1)
            .iter()
            .map(|e| e.location)
            .collect();
        self.register_location_checks(key, &all, out);
        self.update_checked_locations(key, out);
    }

    /// `collect_player` (`MultiServer.py:1101-1118`): pull in everything the
    /// rest of the multiworld is still holding for this slot.
    ///
    /// The reverse of a release — it checks *other* players' locations, the
    /// ones that happen to contain this slot's items.
    pub fn collect_player(&mut self, key: SlotKey, out: &mut dyn EffectSink) {
        self.collect_inner(key, false, out);
    }

    fn collect_inner(&mut self, key: SlotKey, is_group: bool, out: &mut dyn EffectSink) {
        let (team, slot) = key;
        let text = format!(
            "{} (Team #{}) has collected their items from other worlds.",
            self.slot_name(key),
            team + 1
        );
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Collect),
                team: Some(team),
                slot: Some(slot),
                ..Default::default()
            })],
        );

        // Group by the world the items are sitting in, so each source slot is
        // registered once and its owner sees one update.
        let mut by_source: BTreeMap<u32, Vec<i64>> = BTreeMap::new();
        for entry in self.data.locations.all() {
            if entry.receiver == slot {
                by_source
                    .entry(entry.sender)
                    .or_default()
                    .push(entry.location);
            }
        }
        for (source, locations) in by_source {
            let source_key = (team, source);
            self.register_location_checks(source_key, &locations, out);
            self.update_checked_locations(source_key, out);
        }

        if is_group {
            return;
        }
        // A group's own slot collects only once every member has, since until
        // then the group is still owed items.
        let groups: Vec<(u32, Vec<u32>)> = self
            .data
            .slot_info
            .iter()
            .filter(|(_, info)| info.group_members.contains(&slot))
            .map(|(id, info)| (*id, info.group_members.clone()))
            .collect();
        for (group, members) in groups {
            let collected = self.group_collected.entry(group).or_default();
            collected.insert(slot);
            if members.iter().all(|m| collected.contains(m)) {
                self.collect_inner((team, group), true, out);
            }
        }
    }

    /// Grant one slot a release regardless of `release_mode`.
    pub fn allow_release(&mut self, key: SlotKey, allowed: bool) {
        if allowed {
            self.allow_releases.insert(key);
        } else {
            self.allow_releases.remove(&key);
        }
    }

    pub(crate) fn release_allowed(&self, key: SlotKey) -> bool {
        self.allow_releases.contains(&key)
    }

    // --- countdown -------------------------------------------------------

    /// Start or retarget the countdown.
    ///
    /// Restarting while one is running only changes the target, exactly as the
    /// reference does — the original loop keeps ticking against the new number
    /// rather than a second one starting alongside it.
    pub(crate) fn start_countdown(&mut self, seconds: i64, now: f64, out: &mut dyn EffectSink) {
        self.countdown_message(
            format!("[Server]: Starting countdown of {seconds}s"),
            seconds,
            out,
        );

        if let Some(running) = &mut self.countdown {
            running.remaining = seconds;
            return;
        }
        if seconds > 0 {
            // The reference's loop announces before its first sleep, so the
            // opening number lands immediately.
            self.countdown_message(format!("[Server]: {seconds}"), seconds, out);
            self.countdown = Some(Countdown {
                remaining: seconds - 1,
                next_at: now + 1.0,
            });
        } else {
            self.countdown_message("[Server]: GO".to_string(), 0, out);
        }
    }

    fn countdown_message(&self, text: String, value: i64, out: &mut dyn EffectSink) {
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Countdown),
                countdown: Some(value),
                ..Default::default()
            })],
        );
    }

    /// When the room next wants [`Room::tick`] called, if ever.
    ///
    /// `None` means nothing is pending and the transport can sleep until a
    /// client says something.
    pub fn next_tick(&self) -> Option<f64> {
        self.countdown.map(|c| c.next_at)
    }

    /// Advance anything time-driven. Idempotent and safe to call early.
    ///
    /// Loops rather than doing one step, so a late tick — a stalled thread, a
    /// suspended container — catches up instead of stretching the countdown.
    pub fn tick(&mut self, now: f64, out: &mut dyn EffectSink) {
        self.clock = now;
        while let Some(state) = self.countdown {
            if now < state.next_at {
                return;
            }
            if state.remaining > 0 {
                self.countdown_message(
                    format!("[Server]: {}", state.remaining),
                    state.remaining,
                    out,
                );
                self.countdown = Some(Countdown {
                    remaining: state.remaining - 1,
                    next_at: state.next_at + 1.0,
                });
            } else {
                self.countdown_message("[Server]: GO".to_string(), 0, out);
                self.countdown = None;
            }
        }
    }

    /// Queue an item for a slot, expanding item-link groups to their members.
    ///
    /// The `remote_items == false` queue skips items a slot sends to itself,
    /// because such a client applies those locally (`MultiServer.py:1126-1131`).
    fn send_item_to(&mut self, team: u32, target: u32, item: NetworkItem) {
        let members = self.group_members_of(target);
        if members.is_empty() {
            if item.player != target {
                Arc::make_mut(
                    self.received_items
                        .entry((team, target, false))
                        .or_default(),
                )
                .push(item);
            }
            Arc::make_mut(self.received_items.entry((team, target, true)).or_default()).push(item);
        } else {
            for member in members {
                self.send_item_to(team, member, item);
            }
        }
    }

    fn group_members_of(&self, slot: u32) -> Vec<u32> {
        self.data
            .slot_info
            .get(&slot)
            .filter(|s| s.slot_type == SlotType::Group)
            .map(|s| s.group_members.clone())
            .unwrap_or_default()
    }

    /// Flush queued items to the connections of the given slots only.
    fn send_new_items(&mut self, slots: &HashSet<u32>, out: &mut dyn EffectSink) {
        for &slot in slots {
            let key = (0u32, slot);
            let Some(conns) = self.by_slot.get(&key) else {
                continue;
            };
            for conn in conns.clone() {
                let Some(client) = self.clients.get(&conn) else {
                    continue;
                };
                if client.items_handling.no_items() {
                    continue;
                }
                let all = self.items_for(conn);
                let index = client.send_index;
                if all.len() <= index {
                    continue;
                }
                let new = all[index..].to_vec();
                out.send(
                    conn,
                    &[ServerPacket::ReceivedItems(ReceivedItems {
                        index,
                        items: new,
                    })],
                );
                if let Some(c) = self.clients.get_mut(&conn) {
                    c.send_index = all.len();
                }
            }
        }
    }

    /// Start inventory followed by the queued items, as one list.
    ///
    /// Start-inventory entries use the sentinel `location = -2, player = 0`
    /// (`MultiServer.py:546-547`).
    fn items_for(&self, conn: ConnId) -> Vec<NetworkItem> {
        let Some(client) = self.clients.get(&conn) else {
            return Vec::new();
        };
        let key = (
            client.team,
            client.slot,
            client.items_handling.remote_items(),
        );

        let mut out = Vec::new();
        if client.items_handling.remote_start_inventory()
            && let Some(codes) = self.data.precollected_items.get(&client.slot)
        {
            out.extend(codes.iter().map(|&item| NetworkItem {
                item,
                location: -2,
                player: 0,
                flags: 0,
            }));
        }
        if let Some(queued) = self.received_items.get(&key) {
            out.extend_from_slice(queued);
        }
        out
    }

    /// `json_format_send_event` (`MultiServer.py:1278-1296`).
    ///
    /// Ids, not names: the server sends `item_id`/`location_id` parts and each
    /// client resolves them against its own cached data package. That is also
    /// why this is cheap enough to run 400k times in a mass release — no name
    /// lookups and no per-item string building on the hot path.
    pub fn item_send_message(receiver: u32, item: NetworkItem) -> ServerPacket {
        let sender = item.player;
        let mut data = vec![JsonMessagePart::player_id(sender)];
        if sender == receiver {
            data.push(JsonMessagePart::text(" found their "));
            data.push(JsonMessagePart::item_id(item.item, sender, item.flags));
        } else {
            data.push(JsonMessagePart::text(" sent "));
            data.push(JsonMessagePart::item_id(item.item, receiver, item.flags));
            data.push(JsonMessagePart::text(" to "));
            data.push(JsonMessagePart::player_id(receiver));
        }
        data.push(JsonMessagePart::text(" ("));
        data.push(JsonMessagePart::location_id(item.location, sender));
        data.push(JsonMessagePart::text(")"));

        ServerPacket::PrintJSON(PrintJson {
            data,
            print_type: Some(PrintJsonType::ItemSend),
            receiving: Some(receiver),
            item: Some(item),
            ..Default::default()
        })
    }

    /// `Hint.as_network_message` (`NetUtils.py:421-441`).
    pub fn hint_message(hint: &Hint) -> ServerPacket {
        let mut data = vec![
            JsonMessagePart::text("[Hint]: "),
            JsonMessagePart::player_id(hint.receiving_player),
            JsonMessagePart::text("'s "),
            JsonMessagePart::item_id(hint.item, hint.receiving_player, hint.item_flags),
            JsonMessagePart::text(" is at "),
            JsonMessagePart::location_id(hint.location, hint.finding_player),
            JsonMessagePart::text(" in "),
            JsonMessagePart::player_id(hint.finding_player),
        ];
        if hint.entrance.is_empty() {
            data.push(JsonMessagePart::text("'s World"));
        } else {
            data.push(JsonMessagePart::text("'s World at "));
            data.push(JsonMessagePart::typed(
                "entrance_name",
                hint.entrance.as_str(),
            ));
        }
        data.push(JsonMessagePart::text(". "));
        data.push(JsonMessagePart::hint_status(hint.status));

        ServerPacket::PrintJSON(PrintJson {
            data,
            print_type: Some(PrintJsonType::Hint),
            receiving: Some(hint.receiving_player),
            // `player` here is the *finding* player: the item is described by
            // where it sits, not by who gets it.
            item: Some(NetworkItem {
                item: hint.item,
                location: hint.location,
                player: hint.finding_player,
                flags: hint.item_flags,
            }),
            found: Some(hint.found),
            ..Default::default()
        })
    }

    // --- hints -----------------------------------------------------------

    /// The slots a hint for `slot` concerns: an item-link group expands to its
    /// members, anything else is just itself (`MultiServer.py:775-778`).
    fn slot_set(&self, slot: u32) -> Vec<u32> {
        let members = self.group_members_of(slot);
        if members.is_empty() {
            vec![slot]
        } else {
            members
        }
    }

    /// Send and remember hints (`MultiServer.py:805-843`).
    ///
    /// `only_new` drops hints the finding player already holds; without it an
    /// existing hint is re-announced but not re-stored. `persist_even_if_found`
    /// is what separates a scout — which remembers everything — from `!hint`,
    /// which does not bank a hint for a location that was already checked.
    /// `recipients`, when given, restricts *delivery* without restricting what
    /// gets stored.
    pub fn notify_hints(
        &mut self,
        team: u32,
        hints: Vec<Hint>,
        only_new: bool,
        persist_even_if_found: bool,
        recipients: Option<&[u32]>,
        out: &mut dyn EffectSink,
    ) {
        let mut hints: Vec<Hint> = if only_new {
            hints
                .into_iter()
                .filter(|h| !self.hints.contains((team, h.finding_player), &h.identity()))
                .collect()
        } else {
            hints
        };
        if hints.is_empty() {
            return;
        }

        // Found hints first, stably, so a scout of a partly-completed world
        // reads in the same order the reference produces.
        hints.sort_by_key(|h| !h.found);

        // Which slots hear about which hints, and in what order.
        let mut concerns: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        let mut new_hint_events: HashSet<u32> = HashSet::new();

        for (index, hint) in hints.iter().enumerate() {
            for player in self.slot_set(hint.receiving_player) {
                concerns.entry(player).or_default().push(index);
            }
            // The finder hears about it too, unless it is also the receiver —
            // or already got this hint above as a member of the receiving group.
            if hint.receiving_player != hint.finding_player {
                let list = concerns.entry(hint.finding_player).or_default();
                if !list.contains(&index) {
                    list.push(index);
                }
            }

            if !hint.found || persist_even_if_found {
                let finder_key = (team, hint.finding_player);
                if !self.hints.contains(finder_key, &hint.identity()) {
                    self.hints.upsert(finder_key, hint.clone());
                    new_hint_events.insert(hint.finding_player);
                    for player in self.slot_set(hint.receiving_player) {
                        self.hints.upsert((team, player), hint.clone());
                        new_hint_events.insert(player);
                    }
                }
            }
        }

        let mut events: Vec<u32> = new_hint_events.into_iter().collect();
        events.sort_unstable();
        // Only a hint that was actually banked changes anything worth saving;
        // re-announcing one the player already holds does not.
        if !events.is_empty() {
            out.mark_dirty();
        }
        for slot in events {
            self.on_new_hint((team, slot), out);
        }

        for (slot, mut indexes) in concerns {
            if recipients.is_some_and(|r| !r.contains(&slot)) {
                continue;
            }
            if self.by_slot.get(&(team, slot)).is_none_or(Vec::is_empty) {
                continue;
            }
            // Hints this slot finds come first — stably, so the found-first
            // order above survives inside each group.
            indexes.sort_by_key(|&i| hints[i].finding_player != slot);
            let msgs: Vec<ServerPacket> = indexes
                .iter()
                .map(|&i| Self::hint_message(&hints[i]))
                .collect();
            out.broadcast(Recipients::SlotText((team, slot)), &msgs);
        }
    }

    /// `LocationScouts` (`MultiServer.py:2016-2035`).
    ///
    /// Answers with what each location holds, and optionally banks a hint for
    /// each. `create_as_hint` is a three-way switch: 0 scouts silently, 1
    /// announces every resulting hint, 2 announces only the ones that did not
    /// already exist. Scout-created hints persist even for locations already
    /// checked, which is what separates a scout from `!hint`.
    fn handle_location_scouts(
        &mut self,
        conn: ConnId,
        args: cmd::LocationScouts,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let (team, slot) = (client.team, client.slot);

        let mut locations = Vec::with_capacity(args.locations.len());
        let mut hints = Vec::new();
        for &location in &args.locations {
            // The reference indexes its location table directly, so an id this
            // slot does not own raises and drops the socket.
            let Some(entry) = self.data.locations.get(slot, location) else {
                self.protocol_error(
                    conn,
                    format!("slot {slot}: LocationScouts for unknown location {location}"),
                    out,
                );
                return;
            };
            let entry = *entry;
            if args.create_as_hint != 0 {
                hints.extend(self.collect_location_hints(
                    (team, slot),
                    slot,
                    location,
                    Some(HintStatus::Unspecified),
                ));
            }
            // `player` is the *receiving* player here — inverted from every
            // other use of `NetworkItem` (`NetUtils.py:93-94`).
            locations.push(NetworkItem {
                item: entry.item,
                location,
                player: entry.receiver,
                flags: entry.flags,
            });
        }

        self.notify_hints(team, hints, args.create_as_hint == 2, true, None, out);
        if !locations.is_empty() && args.create_as_hint != 0 {
            out.mark_dirty();
        }
        out.send(
            conn,
            &[ServerPacket::LocationInfo(LocationInfo { locations })],
        );
    }

    /// `CreateHints` (`MultiServer.py:2037-2087`).
    ///
    /// Hints without spending points, which is why the permission rules are the
    /// interesting part: a slot may hint freely inside its own world, and may
    /// hint another slot's location only for an item destined to itself — and
    /// then only with the "unspecified" status, so it cannot editorialize about
    /// someone else's item.
    fn handle_create_hints(
        &mut self,
        conn: ConnId,
        args: cmd::CreateHints,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let (team, slot) = (client.team, client.slot);
        let location_player = args.player.unwrap_or(slot);

        if args.locations.is_empty() {
            self.bad_arguments(
                conn,
                "CreateHints",
                "CreateHints: No locations specified.".into(),
                out,
            );
            return;
        }

        // An absent status means "unspecified". An explicit null is a
        // divergence we accept: Python rejects it as an unknown status, but the
        // decoded form cannot tell the two apart and no client sends null here.
        let status = match args.status {
            None => HintStatus::Unspecified,
            Some(raw) => match HintStatus::from_i64(raw, &pahoa_multidata::Path::root()) {
                Ok(s) => s,
                Err(_) => {
                    self.bad_arguments(
                        conn,
                        "CreateHints",
                        format!("Unknown Status: {raw} is not a valid HintStatus"),
                        out,
                    );
                    return;
                }
            },
        };

        let mut hints = Vec::new();
        for &location in &args.locations {
            let entry = match self.data.locations.get(location_player, location) {
                Some(e) => *e,
                None if location_player != slot => {
                    self.bad_arguments(
                        conn,
                        "CreateHints",
                        "CreateHints: One or more of the locations do not exist for the \
                         specified off-world player. Please refrain from hinting other slot's \
                         locations that you don't know contain your items."
                            .into(),
                        out,
                    );
                    return;
                }
                // Own slot, unknown location: the reference indexes and raises.
                None => {
                    self.protocol_error(
                        conn,
                        format!("slot {slot}: CreateHints for unknown location {location}"),
                        out,
                    );
                    return;
                }
            };

            if !self.slot_set(entry.receiver).contains(&slot) {
                if status != HintStatus::Unspecified {
                    self.bad_arguments(
                        conn,
                        "CreateHints",
                        "CreateHints: Must use \"unspecified\"/None status for items from \
                         other players."
                            .into(),
                        out,
                    );
                    return;
                }
                if slot != location_player {
                    self.bad_arguments(
                        conn,
                        "CreateHints",
                        "CreateHints: Can only create hints for own items or own locations.".into(),
                        out,
                    );
                    return;
                }
            }

            hints.extend(self.collect_location_hints(
                (team, location_player),
                location_player,
                location,
                Some(status),
            ));
        }

        self.notify_hints(team, hints, true, true, None, out);
        out.mark_dirty();
    }

    /// `UpdateHint` (`MultiServer.py:2089-2129`).
    ///
    /// Only the receiving player may reprioritize a hint, and nobody may set
    /// "found" by hand — that flag is derived from the location actually being
    /// checked.
    fn handle_update_hint(
        &mut self,
        conn: ConnId,
        args: cmd::UpdateHint,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let (team, slot) = (client.team, client.slot);

        // Missing hints are ignored rather than refused: a client may be
        // working from a stale list.
        let Some(hint) = self
            .hints
            .find((team, args.player), args.player, args.location)
            .cloned()
        else {
            return;
        };

        if !self.slot_set(hint.receiving_player).contains(&slot) {
            self.bad_arguments(conn, "UpdateHint", "UpdateHint: No Permission".into(), out);
            return;
        }

        let Some(raw) = args.status else {
            return;
        };
        let Ok(status) = HintStatus::from_i64(raw, &pahoa_multidata::Path::root()) else {
            self.bad_arguments(conn, "UpdateHint", "UpdateHint: Invalid Status".into(), out);
            return;
        };
        if status == HintStatus::Found {
            self.bad_arguments(
                conn,
                "UpdateHint",
                "UpdateHint: Cannot manually update status to \"HINT_FOUND\"".into(),
                out,
            );
            return;
        }

        // `re_prioritize`: a found hint keeps its status whatever was asked for.
        let effective = if hint.found {
            HintStatus::Found
        } else {
            status
        };
        if effective == hint.status {
            return;
        }

        let mut concerned = self.slot_set(hint.receiving_player);
        concerned.push(hint.finding_player);
        concerned.sort_unstable();
        concerned.dedup();

        for &target in &concerned {
            self.hints
                .set_status((team, target), hint.finding_player, hint.location, status);
        }
        out.mark_dirty();
        for target in concerned {
            self.on_changed_hints((team, target), out);
        }
    }

    /// One location's hint, reusing an existing one so its status survives
    /// (`MultiServer.py:1231-1256`).
    fn collect_location_hints(
        &self,
        key: SlotKey,
        slot: u32,
        location: i64,
        status: Option<HintStatus>,
    ) -> Vec<Hint> {
        crate::hints::collect_for_location(
            &self.data,
            &self.hints,
            key,
            slot,
            location,
            status,
            &|s, loc| {
                self.location_checks
                    .get(&(key.0, s))
                    .is_some_and(|c| c.contains(&loc))
            },
        )
    }

    /// A hint was banked: the slot's point balance moved, so tell it
    /// (`MultiServer.py:872-877`).
    fn on_new_hint(&mut self, key: SlotKey, out: &mut dyn EffectSink) {
        self.on_changed_hints(key, out);
        out.broadcast(
            Recipients::Slot(key),
            &[ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                hint_points: Some(self.slot_points(key)),
                ..Default::default()
            }))],
        );
    }

    /// Push the slot's whole hint list to anything subscribed to its
    /// `_read_hints_*` key — how trackers stay current.
    ///
    /// Built as a bare map rather than through [`ServerPacket::echo`]: the
    /// reference constructs a fresh dict here, so the reply carries only `cmd`,
    /// `key` and `value`, with none of the `original_value`/`slot` fields a
    /// client-initiated `Set` would produce.
    fn on_changed_hints(&self, key: SlotKey, out: &mut dyn EffectSink) {
        let name = format!("_read_hints_{}_{}", key.0, key.1);
        let Some(subscribers) = self.stored_data_subscriptions.get(&name) else {
            return;
        };
        if subscribers.is_empty() {
            return;
        }
        let mut targets: Vec<ConnId> = subscribers.iter().copied().collect();
        targets.sort_unstable();

        let mut reply = Map::new();
        reply.insert("cmd".into(), Value::from("SetReply"));
        reply.insert("key".into(), Value::from(name));
        reply.insert("value".into(), self.hints_json(key));
        out.broadcast(Recipients::These(targets), &[ServerPacket::Echo(reply)]);
    }

    /// One slot's hints, as the `_read_hints_*` key exposes them.
    fn hints_json(&self, key: SlotKey) -> Value {
        Value::Array(
            self.hints
                .get(key)
                .iter()
                .map(|h| serde_json::to_value(pahoa_proto::Hint::from(h)).expect("hints serialize"))
                .collect(),
        )
    }

    pub fn hints_for(&self, key: SlotKey) -> &[Hint] {
        self.hints.get(key)
    }

    /// Replace a slot's hints wholesale.
    ///
    /// For restoring a save, and for an administrator clearing a slot's list.
    pub fn set_hints(&mut self, key: SlotKey, hints: Vec<Hint>) {
        self.hints.replace(key, hints);
    }

    /// How many hints a slot has paid for, which is what `hint_points` deducts.
    pub fn hints_used(&self, key: SlotKey) -> i64 {
        self.hints_used.get(&key).copied().unwrap_or(0)
    }

    // --- derived views ---------------------------------------------------

    fn room_info(&self) -> RoomInfo {
        let mut games: Vec<String> = self.data.games().into_iter().collect();
        games.sort();

        RoomInfo {
            version: SERVER_VERSION,
            generator_version: Version::from(self.data.generator_version),
            tags: self.options.tags.clone(),
            // "This room will ask you for a password", not "which mode it
            // uses". `RoomInfo` goes out before the slot name is known, so a
            // per-slot password cannot be reported per slot — and reporting
            // `false` would stop a client prompting for one it does need.
            password: self.options.password.is_some() || !self.options.slot_passwords.is_empty(),
            permissions: BTreeMap::from([
                ("release".to_string(), self.options.release_mode),
                ("collect".to_string(), self.options.collect_mode),
                ("remaining".to_string(), self.options.remaining_mode),
            ]),
            hint_cost: self.options.hint_cost,
            location_check_points: self.options.location_check_points,
            games,
            datapackage_checksums: self
                .datapackage
                .checksums()
                .into_iter()
                .map(|(g, c)| (g.to_string(), c.to_string()))
                .collect(),
            seed_name: self.data.seed_name.clone(),
            time: self.start_time,
        }
    }

    fn players_package(&self) -> Vec<NetworkPlayer> {
        self.data
            .slot_info
            .iter()
            .filter(|(_, info)| info.slot_type == SlotType::Player)
            .map(|(slot, info)| NetworkPlayer {
                team: 0,
                slot: *slot,
                alias: self.slot_alias((0, *slot)),
                name: info.name.clone(),
            })
            .collect()
    }

    fn slot_info_package(&self) -> BTreeMap<String, NetworkSlot> {
        self.data
            .slot_info
            .iter()
            .map(|(slot, info)| (slot.to_string(), NetworkSlot::from_multidata(info)))
            .collect()
    }

    /// The slot's immutable name from the seed.
    pub(crate) fn slot_name(&self, key: SlotKey) -> String {
        self.data
            .slot_info
            .get(&key.1)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| format!("Unknown slot {}", key.1))
    }

    /// `get_aliased_name` (`MultiServer.py:799-803`): how a slot is written in
    /// chat and in `NetworkPlayer.alias`.
    ///
    /// An alias does not *replace* the seed name, it prefixes it — `"Bob
    /// (SlotName)"` — so other players can still tell who is who.
    pub(crate) fn slot_alias(&self, key: SlotKey) -> String {
        let name = self.slot_name(key);
        match self.name_aliases.get(&key) {
            Some(alias) => format!("{alias} ({name})"),
            None => name,
        }
    }

    fn slot_game(&self, slot: u32) -> String {
        self.data
            .slot_info
            .get(&slot)
            .map(|i| i.game.clone())
            .unwrap_or_else(|| "Archipelago".to_string())
    }

    fn slot_data_json(&self, slot: u32) -> Option<Box<serde_json::value::RawValue>> {
        self.data
            .slot_data
            .get(&slot)
            .map(crate::slot_data::to_json)
    }

    /// How many locations a slot has checked.
    pub fn checked_count(&self, key: SlotKey) -> usize {
        self.location_checks.get(&key).map_or(0, |c| c.len())
    }

    fn checked_locations(&self, key: SlotKey) -> Vec<i64> {
        let mut v: Vec<i64> = self
            .location_checks
            .get(&key)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    fn missing_locations(&self, key: SlotKey) -> Vec<i64> {
        let checked = self.location_checks.get(&key);
        self.data
            .locations
            .for_slot(key.1)
            .iter()
            .map(|e| e.location)
            .filter(|loc| checked.is_none_or(|c| !c.contains(loc)))
            .collect()
    }

    /// `location_check_points * checks - hint_cost_absolute * hints_used`
    /// (`MultiServer.py:1845-1852`).
    pub fn slot_points(&self, key: SlotKey) -> i64 {
        let checks = self.checked_count(key) as i64;
        let total = self.data.locations.count_for(key.1);
        let used = self.hints_used.get(&key).copied().unwrap_or(0);
        self.options.location_check_points as i64 * checks
            - self.options.hint_cost_for(total) * used
    }

    /// Connections that should receive text broadcasts, i.e. everyone
    /// authenticated without the `NoText` tag.
    /// Expand a [`Recipients`] into concrete connections.
    ///
    /// The production transport keeps its own membership indexes and expands
    /// these off the actor's critical path; this exists for tests and for any
    /// caller that only has the room. Sorted, so effect streams stay comparable
    /// between runs.
    pub fn resolve(&self, to: &Recipients) -> Vec<ConnId> {
        let mut v: Vec<ConnId> = match to {
            Recipients::All => self
                .clients
                .values()
                .filter(|c| c.auth)
                .map(|c| c.id)
                .collect(),
            Recipients::AllText => self
                .clients
                .values()
                .filter(|c| c.auth && !c.no_text)
                .map(|c| c.id)
                .collect(),
            Recipients::Slot(key) => self.by_slot.get(key).cloned().unwrap_or_default(),
            Recipients::SlotText(key) => self
                .by_slot
                .get(key)
                .into_iter()
                .flatten()
                .filter(|c| self.clients.get(c).is_some_and(|c| !c.no_text))
                .copied()
                .collect(),
            Recipients::These(list) => list.clone(),
        };
        v.sort_unstable();
        v
    }

    /// All authenticated connections.
    pub fn all_conns(&self) -> Vec<ConnId> {
        self.resolve(&Recipients::All)
    }

    /// The key-value store, for tests. Saving goes through [`Room::snapshot`].
    pub fn stored_data(&self) -> &HashMap<String, Arc<Value>> {
        &self.stored_data
    }

    // --- persistence -----------------------------------------------------

    /// Take a consistent point-in-time copy of everything persistent.
    ///
    /// Runs on the actor, so it must stay O(slots): every bulky field is an
    /// `Arc` clone. Nothing here allocates per location, per item or per hint,
    /// which is what keeps a save off the room's critical path however slow the
    /// backing store turns out to be.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            seed_name: self.data.seed_name.clone(),
            options: self.options.clone(),
            rng_state: self.rng.to_state().to_vec(),
            location_checks: self
                .location_checks
                .iter()
                .map(|(k, v)| (*k, Arc::clone(v)))
                .collect(),
            received_items: self
                .received_items
                .iter()
                .map(|(k, v)| (*k, Arc::clone(v)))
                .collect(),
            hints: self
                .hints
                .slots()
                .map(|(k, v)| (*k, Arc::clone(v)))
                .collect(),
            hints_used: self.hints_used.iter().map(|(k, v)| (*k, *v)).collect(),
            name_aliases: self
                .name_aliases
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
            client_game_state: self
                .client_game_state
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect(),
            group_collected: self
                .group_collected
                .iter()
                .map(|(k, v)| (*k, v.iter().copied().collect()))
                .collect(),
            allow_releases: self.allow_releases.iter().copied().collect(),
            stored_data: self
                .stored_data
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
        }
    }

    /// Restore a snapshot over a freshly constructed room.
    ///
    /// Refuses a save from a different seed. The reference compares
    /// `connect_names` and raises (`MultiServer.py:686-687`); refusing either
    /// way is right, because one seed's checks against another's location table
    /// would present as corruption rather than as an error.
    ///
    /// Everything the save carries is replaced wholesale rather than merged.
    /// The reference `update`s these maps onto whatever the fresh room built,
    /// which differs only for slots the save does not mention — and since every
    /// map is keyed by slot, both approaches leave those untouched.
    ///
    /// Live connections are deliberately not part of a save: a restored room
    /// starts with nobody attached, and clients resync on `Connect`.
    pub fn restore(&mut self, snapshot: Snapshot) -> Result<(), SaveError> {
        if snapshot.seed_name != self.data.seed_name {
            return Err(SaveError::WrongSeed {
                expected: self.data.seed_name.clone(),
                found: snapshot.seed_name,
            });
        }
        let Some(rng) = PyRandom::from_state(&snapshot.rng_state) else {
            return Err(SaveError::Malformed("random state is the wrong shape"));
        };

        // Everything except the secrets, which the save deliberately does not
        // carry (see `save::encode_options`). Restoring wholesale would replace
        // the configured passwords with the `None` the decoder produces, which
        // is the same bug in the other direction: a room that quietly stopped
        // asking for the password it was started with.
        self.options = RoomOptions {
            password: self.options.password.take(),
            server_password: self.options.server_password.take(),
            slot_passwords: std::mem::take(&mut self.options.slot_passwords),
            ..snapshot.options
        };
        self.rng = rng;
        self.location_checks = snapshot.location_checks.into_iter().collect();
        self.received_items = snapshot.received_items.into_iter().collect();
        self.hints = HintStore::default();
        for (key, list) in snapshot.hints {
            self.hints
                .replace(key, Arc::try_unwrap(list).unwrap_or_else(|a| (*a).clone()));
        }
        self.hints_used = snapshot.hints_used.into_iter().collect();
        self.name_aliases = snapshot.name_aliases.into_iter().collect();
        self.client_game_state = snapshot.client_game_state.into_iter().collect();
        self.group_collected = snapshot
            .group_collected
            .into_iter()
            .map(|(group, members)| (group, members.into_iter().collect()))
            .collect();
        self.allow_releases = snapshot.allow_releases.into_iter().collect();
        self.stored_data = snapshot.stored_data.into_iter().collect();

        Ok(())
    }

    pub fn shutdown(&mut self, out: &mut dyn EffectSink) {
        for conn in self.clients.keys().copied().collect::<Vec<_>>() {
            out.close(conn, CloseReason::ServerShutdown);
        }
        self.clients.clear();
        self.by_slot.clear();
    }
}

/// Parse the `{team}_{slot}` suffix of a `_read_` key.
fn parse_team_slot(s: &str) -> Option<(u32, u32)> {
    let (team, slot) = s.split_once('_')?;
    Some((team.parse().ok()?, slot.parse().ok()?))
}

/// Name groups as the `_read_*_name_groups_*` keys expose them.
///
/// An unknown game yields an empty object rather than null, matching how
/// Python's `KeyedDefaultDict` behaves for a missing entry.
fn groups_to_json(groups: Option<&BTreeMap<String, Vec<String>>>) -> Value {
    match groups {
        Some(g) => Value::Object(
            g.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Value::Array(v.iter().cloned().map(Value::from).collect()),
                    )
                })
                .collect(),
        ),
        None => Value::Object(Map::new()),
    }
}
