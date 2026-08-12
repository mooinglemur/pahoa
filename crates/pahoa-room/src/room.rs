//! The room state machine.
//!
//! Owns everything mutable about a live multiworld and produces outbound
//! packets through an [`EffectSink`]. No sockets, no clock beyond what callers
//! pass in, no async.

use crate::conn::{Client, ConnId, non_game_verb};
use crate::effect::{CloseReason, EffectSink, Recipients};
use crate::options::RoomOptions;
use pahoa_multidata::{DataPackage as NameTables, MultiData, SlotType};
use pahoa_proto::server::*;
use pahoa_proto::types::*;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Server version reported in `RoomInfo`. Tracks the Archipelago release whose
/// behaviour this implementation reproduces.
pub const SERVER_VERSION: Version = Version::new(0, 6, 8);

/// `PrintJSON` packets per broadcast frame.
///
/// Chosen by Archipelago to sit near the compression window without producing
/// oversized frames (`MultiServer.py:1165-1167`); reproducing it keeps
/// bandwidth behaviour and frame counts comparable.
const PRINT_JSON_CHUNK: usize = 140;

/// `(team, slot)`. Teams beyond 0 never occur today — the reference server only
/// ever creates team 0 — but the key shape is everywhere in the protocol and
/// retrofitting it later would touch every structure, so it is kept from the start.
pub type SlotKey = (u32, u32);

pub struct Room {
    data: Arc<MultiData>,
    datapackage: Arc<NameTables>,
    pub options: RoomOptions,

    clients: HashMap<ConnId, Client>,
    /// Connections currently authenticated to each slot. Co-op means this is a
    /// list, not a single entry.
    by_slot: HashMap<SlotKey, Vec<ConnId>>,

    location_checks: HashMap<SlotKey, HashSet<i64>>,
    /// Two queues per slot, keyed on the connection's `remote_items` setting:
    /// a client that does not want its own world's items gets a different
    /// stream from one that does (`MultiServer.py:1126-1131`).
    received_items: HashMap<(u32, u32, bool), Vec<NetworkItem>>,
    client_game_state: HashMap<SlotKey, ClientStatus>,
    name_aliases: HashMap<SlotKey, String>,
    hints_used: HashMap<SlotKey, i64>,

    /// Server start time, reported in `RoomInfo.time` for DeathLink sync.
    pub start_time: f64,
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

        Self {
            data,
            datapackage,
            options,
            clients: HashMap::new(),
            by_slot: HashMap::new(),
            location_checks: HashMap::new(),
            received_items: HashMap::new(),
            client_game_state,
            name_aliases: HashMap::new(),
            hints_used: HashMap::new(),
            start_time,
        }
    }

    pub fn multidata(&self) -> &MultiData {
        &self.data
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

        // Only announce the departure once the slot has nobody left, and only
        // reset status when the last connection goes (`MultiServer.py:990-1007`).
        if self.by_slot.get(&key).is_none_or(Vec::is_empty) {
            self.client_game_state
                .entry(key)
                .and_modify(|s| {
                    if *s != ClientStatus::Goal {
                        *s = ClientStatus::Unknown;
                    }
                })
                .or_insert(ClientStatus::Unknown);

            let verb = non_game_verb(&client.tags).unwrap_or("playing");
            let text = format!(
                "{} ({}) has left the game. ({verb})",
                self.slot_alias(key),
                self.slot_game(client.slot),
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
            // Hints, data storage, scouts, chat and bounce arrive in later
            // milestones; ignoring them here matches the pre-auth behaviour of
            // an unknown-but-valid command rather than inventing a refusal.
            _ => {}
        }
    }

    // --- Connect ---------------------------------------------------------

    fn handle_connect(&mut self, conn: ConnId, args: cmd::Connect, out: &mut dyn EffectSink) {
        let mut errors: Vec<ConnectionRefusedReason> = Vec::new();

        if let Some(expected) = &self.options.password
            && args.password.as_deref() != Some(expected.as_str())
        {
            errors.push(ConnectionRefusedReason::InvalidPassword);
        }

        let resolved = self.data.connect_names.get(&args.name).copied();
        let mut items_handling = ItemsHandling::new(0).expect("0 is valid");

        match resolved {
            None => errors.push(ConnectionRefusedReason::InvalidSlot),
            Some((_team, slot)) => {
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
            self.announce_join(conn, out);
        }

        out.send(conn, &reply);
    }

    fn announce_join(&self, conn: ConnId, out: &mut dyn EffectSink) {
        let client = &self.clients[&conn];
        let key = (client.team, client.slot);
        let verb = non_game_verb(&client.tags).unwrap_or("playing");
        let text = format!(
            "{} ({}) has joined. Client({}), {}.",
            self.slot_alias(key),
            self.slot_game(client.slot),
            client.version,
            verb,
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
        let wanted: Vec<String> = match (&args.games, &args.exclusions) {
            (Some(games), _) => games.clone(),
            // Deprecated, past its own removal TODO, still honoured.
            (None, Some(excluded)) => self
                .datapackage
                .games()
                .map(|(g, _)| g.clone())
                .filter(|g| !excluded.contains(g))
                .collect(),
            (None, None) => self.datapackage.games().map(|(g, _)| g.clone()).collect(),
        };

        let mut games = BTreeMap::new();
        for game in wanted {
            let Some(names) = self.datapackage.get(&game) else {
                continue;
            };
            games.insert(
                game,
                GameData {
                    item_name_to_id: names.package.item_name_to_id.clone(),
                    location_name_to_id: names.package.location_name_to_id.clone(),
                    checksum: names.package.checksum.clone(),
                },
            );
        }

        out.send(
            conn,
            &[ServerPacket::DataPackage(DataPackage {
                data: DataPackageContents { games },
            })],
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
            feed.push(self.item_send_message(slot, receiver, net));
        }
        if !feed.is_empty() {
            out.broadcast(Recipients::AllText, &feed);
        }

        self.location_checks
            .entry(key)
            .or_default()
            .extend(fresh.iter().copied());

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

        out.mark_dirty();
    }

    /// Queue an item for a slot, expanding item-link groups to their members.
    ///
    /// The `remote_items == false` queue skips items a slot sends to itself,
    /// because such a client applies those locally (`MultiServer.py:1126-1131`).
    fn send_item_to(&mut self, team: u32, target: u32, item: NetworkItem) {
        let members = self.group_members_of(target);
        if members.is_empty() {
            if item.player != target {
                self.received_items
                    .entry((team, target, false))
                    .or_default()
                    .push(item);
            }
            self.received_items
                .entry((team, target, true))
                .or_default()
                .push(item);
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

    fn item_send_message(&self, sender: u32, receiver: u32, item: NetworkItem) -> ServerPacket {
        let sender_game = self.slot_game(sender);
        let receiver_game = self.slot_game(receiver);
        let item_name = self
            .datapackage
            .get(&receiver_game)
            .map(|n| n.item_name(item.item))
            .unwrap_or_else(|| format!("Unknown item (ID:{})", item.item));
        let location_name = self
            .datapackage
            .get(&sender_game)
            .map(|n| n.location_name(item.location))
            .unwrap_or_else(|| format!("Unknown location (ID:{})", item.location));

        ServerPacket::PrintJSON(PrintJson {
            data: vec![
                JsonMessagePart::player_id(sender),
                JsonMessagePart::text(" sent "),
                JsonMessagePart {
                    text: Some(item_name),
                    part_type: Some("item_name".into()),
                    player: Some(receiver),
                    flags: Some(item.flags),
                    ..Default::default()
                },
                JsonMessagePart::text(" to "),
                JsonMessagePart::player_id(receiver),
                JsonMessagePart::text(" ("),
                JsonMessagePart {
                    text: Some(location_name),
                    part_type: Some("location_name".into()),
                    player: Some(sender),
                    ..Default::default()
                },
                JsonMessagePart::text(")"),
            ],
            print_type: Some(PrintJsonType::ItemSend),
            receiving: Some(receiver),
            item: Some(item),
            ..Default::default()
        })
    }

    // --- derived views ---------------------------------------------------

    fn room_info(&self) -> RoomInfo {
        let mut games: Vec<String> = self.data.games().into_iter().collect();
        games.sort();

        RoomInfo {
            version: SERVER_VERSION,
            generator_version: Version::from(self.data.generator_version),
            tags: self.options.tags.clone(),
            password: self.options.password.is_some(),
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

    fn slot_alias(&self, key: SlotKey) -> String {
        self.name_aliases
            .get(&key)
            .cloned()
            .or_else(|| self.data.slot_info.get(&key.1).map(|i| i.name.clone()))
            .unwrap_or_else(|| format!("Unknown slot {}", key.1))
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
        let checks = self.location_checks.get(&key).map_or(0, HashSet::len) as i64;
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
            Recipients::These(list) => list.clone(),
        };
        v.sort_unstable();
        v
    }

    /// All authenticated connections.
    pub fn all_conns(&self) -> Vec<ConnId> {
        self.resolve(&Recipients::All)
    }

    pub fn shutdown(&mut self, out: &mut dyn EffectSink) {
        for conn in self.clients.keys().copied().collect::<Vec<_>>() {
            out.close(conn, CloseReason::ServerShutdown);
        }
        self.clients.clear();
        self.by_slot.clear();
    }
}
