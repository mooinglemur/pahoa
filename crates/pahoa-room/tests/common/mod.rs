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
