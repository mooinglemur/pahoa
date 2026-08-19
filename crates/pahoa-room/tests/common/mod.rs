//! Shared fixtures for room tests.
//!
//! Every test binary compiles this module separately, so a helper only one of
//! them needs reads as dead code in all the others.
#![allow(dead_code)]

use pahoa_multidata::{GamePackage, MultiData};
use pahoa_proto::types::Version;
use pahoa_proto::{ClientPacket, client as cmd};
use pahoa_room::{ConnId, Room, RoomOptions};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub fn fixture_dir() -> PathBuf {
    std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crate is two levels below the workspace root")
                .join("crates/pahoa-pickle/tests/fixtures")
        })
}

/// Load a named fixture, or `None` when fixtures are not installed.
pub fn load(name: &str) -> Option<Arc<MultiData>> {
    let path = fixture_dir().join(name);
    let raw = std::fs::read(&path).ok()?;
    Some(Arc::new(
        MultiData::parse(&raw).expect("fixture should parse"),
    ))
}

/// Build a room over a fixture, resolving names from the embedded package only
/// (no snapshot — these tests do not exercise hint blacklists).
pub fn room_for(data: Arc<MultiData>, options: RoomOptions) -> Room {
    let snapshot: BTreeMap<String, GamePackage> = BTreeMap::new();
    let (names, _) = data.resolve_datapackage(&snapshot);
    Room::new(data, Arc::new(names), options, 1_700_000_000.0)
}

/// The player slot owed the most placements, as `(slot, name, game)`.
///
/// For tests that need a *pool* rather than merely a player: a shuffle over two
/// candidates matches an unshuffled one half the time, so a control that says
/// "an untouched room orders these differently" proves nothing there. The hint
/// vector generator picks its subject the same way and for the same reason.
pub fn richest_player(data: &MultiData) -> (u32, String, String) {
    let mut owed: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for finder in data.slot_info.keys() {
        for entry in data.locations.for_slot(*finder) {
            *owed.entry(entry.receiver).or_default() += 1;
        }
    }
    let (slot, info) = data
        .player_slots()
        .max_by_key(|(s, _)| (owed.get(s).copied().unwrap_or(0), std::cmp::Reverse(**s)))
        .expect("fixture has players");
    (*slot, info.name.clone(), info.game.clone())
}

/// An item name `slot` is owed in several places, for driving `!hint`.
///
/// Derived from the seed rather than written down. A hard-coded item name is a
/// fixture assumption that goes stale silently: the hint simply matches nothing,
/// no hints are granted, and a test about *ordering* fails with "want an order
/// to compare" rather than "that item is not in this seed". The most-owed item
/// is chosen for the same reason `tools/gen-hint-vectors.py` chooses it — so the
/// one-per-call rule has something to pick between.
pub fn most_owed_item(room: &Room, slot: u32) -> Option<String> {
    let data = room.multidata();
    let mut owed: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for finder in data.slot_info.keys() {
        for entry in data.locations.for_slot(*finder) {
            if entry.receiver == slot {
                *owed.entry(entry.item).or_default() += 1;
            }
        }
    }
    // Ties break on the item id, so the choice is stable across runs.
    let (id, _) = owed.into_iter().max_by_key(|(id, n)| (*n, -*id))?;

    let game = data.slot_info.get(&slot)?.game.clone();
    let names = room.datapackage().get(&game)?;
    let name = names.item_name(id);
    // `item_name` falls back to "Unknown item (ID:n)" for an id the package
    // does not cover, which would not match anything either.
    names.item_id(&name).map(|_| name)
}

/// A `Connect` for a slot, with sensible defaults.
pub fn connect(name: &str, game: &str, items_handling: u8) -> ClientPacket {
    ClientPacket::Connect(Box::new(cmd::Connect {
        password: None,
        game: Some(game.to_string()),
        name: name.to_string(),
        uuid: serde_json::json!("test-uuid"),
        version: Version::new(0, 6, 8),
        items_handling,
        tags: vec!["AP".to_string()],
        slot_data: true,
    }))
}

/// The first player slot in the fixture, as `(slot, name, game)`.
pub fn first_player(data: &MultiData) -> (u32, String, String) {
    let (slot, info) = data
        .player_slots()
        .next()
        .expect("fixture has a player slot");
    (*slot, info.name.clone(), info.game.clone())
}

/// Connect a client and return its id, discarding the handshake traffic.
pub fn join(room: &mut Room, id: u64, name: &str, game: &str, items_handling: u8) -> ConnId {
    let conn = ConnId(id);
    let mut sink = pahoa_room::Recorder::default();
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect(name, game, items_handling), &mut sink);
    conn
}

/// Print a skip message and return true when fixtures are missing, so a skipped
/// test never reads as a passing one.
pub fn skip_without(name: &str) -> bool {
    if load(name).is_none() {
        eprintln!(
            "SKIP: fixture {name} not present in {}",
            fixture_dir().display()
        );
        return true;
    }
    false
}

pub fn packet_name(p: &pahoa_proto::ServerPacket) -> &'static str {
    use pahoa_proto::ServerPacket as S;
    match p {
        S::RoomInfo(_) => "RoomInfo",
        S::ConnectionRefused(_) => "ConnectionRefused",
        S::Connected(_) => "Connected",
        S::ReceivedItems(_) => "ReceivedItems",
        S::LocationInfo(_) => "LocationInfo",
        S::RoomUpdate(_) => "RoomUpdate",
        S::PrintJSON(_) => "PrintJSON",
        S::DataPackage(_) => "DataPackage",
        S::InvalidPacket(_) => "InvalidPacket",
        S::Echo(_) => "Echo",
    }
}
