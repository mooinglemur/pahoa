//! Save round-trips, and what a save refuses.
//!
//! The claim under test is that a room reconstructed from a snapshot is
//! *indistinguishable* from the one that produced it — not merely that the
//! fields survive. So most of these drive the restored room and compare what it
//! says to a client, which is the only thing that actually matters.

mod common;

use common::*;
use pahoa_multidata::{Hint, HintStatus};
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::save::{FORMAT_VERSION, SaveError, Snapshot};
use pahoa_room::{Recorder, Room, RoomOptions};
use serde_json::{Map, json};

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

/// A room with a bit of everything in it: checks, items owed, hints, aliases,
/// datastorage, a spent hint budget.
fn played_room() -> (Room, u32, String, String) {
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(
        data.clone(),
        RoomOptions {
            hint_cost: 0,
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    let mut sink = Recorder::default();

    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(40)
        .map(|e| e.location)
        .collect();
    room.register_location_checks((0, slot), &locations, &mut sink);

    room.handle(
        conn,
        ClientPacket::Say(cmd::Say {
            text: "!alias Nickname".to_string(),
        }),
        &mut sink,
    );
    room.handle(
        conn,
        ClientPacket::Say(cmd::Say {
            text: "!hint Additional Palette Color".to_string(),
        }),
        &mut sink,
    );
    // Key order is observable when this comes back out in `Retrieved`, so the
    // round-trip has to preserve it.
    let value = json!({"zebra": 1, "apple": [2, 3], "nested": {"b": 1, "a": 2}});
    room.handle(
        conn,
        ClientPacket::Set(
            Box::new(cmd::Set {
                key: "tracker".to_string(),
                default: None,
                want_reply: false,
                operations: vec![cmd::DataStorageOperation {
                    operation: "replace".to_string(),
                    value: value.clone(),
                }],
            }),
            Map::from_iter([
                ("cmd".to_string(), json!("Set")),
                ("key".to_string(), json!("tracker")),
                (
                    "operations".to_string(),
                    json!([{"operation": "replace", "value": value}]),
                ),
            ]),
        ),
        &mut sink,
    );

    (room, slot, name, game)
}

/// Rebuild from bytes, as a restart would.
fn reload(bytes: &[u8]) -> Room {
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());
    room.restore(Snapshot::decode(bytes).expect("save decodes"))
        .expect("save restores");
    room
}

#[test]
fn a_restored_room_answers_a_client_exactly_as_the_original_would() {
    if skip_without(FIXTURE) {
        return;
    }
    let (room, slot, name, game) = played_room();
    let key = (0, slot);

    let before = room.snapshot();
    let restored = reload(&before.encode(true));

    // Compare through the client-visible surface rather than field by field.
    assert_eq!(restored.checked_count(key), room.checked_count(key));
    assert_eq!(restored.slot_points(key), room.slot_points(key));
    assert_eq!(restored.hints_used(key), room.hints_used(key));
    assert_eq!(restored.hints_for(key), room.hints_for(key));
    assert_eq!(restored.stored_data(), room.stored_data());

    // The alias survives, which is what a returning player sees first.
    assert_eq!(
        restored.snapshot().name_aliases,
        vec![(key, "Nickname".to_string())]
    );

    // And a fresh connection is told the same thing by both rooms.
    let items = |room: &mut Room| {
        let mut sink = Recorder::default();
        let conn = pahoa_room::ConnId(99);
        room.on_connect(conn, &mut sink);
        sink.clear();
        room.handle(conn, connect(&name, &game, 0b111), &mut sink);
        sink.packets_for(conn, room)
            .into_iter()
            .filter_map(|p| match p {
                ServerPacket::ReceivedItems(r) => Some(r.items.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut original = room;
    let mut restored = restored;
    assert_eq!(items(&mut restored), items(&mut original));
}

#[test]
fn the_hint_prng_resumes_where_it_left_off() {
    if skip_without(FIXTURE) {
        return;
    }
    // The point of persisting the random state at all: hint ordering must not
    // reset to the start of the sequence on every restart, or a restart becomes
    // a way to re-roll which hint a player is granted.
    let (mut room, _slot, name, game) = played_room();
    let mut restored = reload(&room.snapshot().encode(false));

    // Read the *announcement* order rather than the store: these placements are
    // already banked under their finders from the hint in `played_room`, and
    // `notify_hints` will not bank a second copy. The order they are announced
    // in is what the shuffle decided, and is what a player sees.
    let next_hints = |room: &mut Room, id: u64| {
        let conn = join(room, id, &name, &game, 0b111);
        let mut sink = Recorder::default();
        room.handle(
            conn,
            ClientPacket::Say(cmd::Say {
                text: "!hint Additional Palette Color".to_string(),
            }),
            &mut sink,
        );
        sink.packets_for(conn, room)
            .into_iter()
            .filter_map(|p| match p {
                ServerPacket::PrintJSON(m)
                    if m.print_type == Some(pahoa_proto::server::PrintJsonType::Hint) =>
                {
                    m.item.map(|i| (i.player, i.location))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    let from_original = next_hints(&mut room, 50);
    let from_restored = next_hints(&mut restored, 51);
    assert!(from_original.len() > 1, "want an order to compare");
    assert_eq!(
        from_restored, from_original,
        "a restored room must continue the shuffle, not restart it"
    );

    // The control, without which the assertion above could pass simply because
    // the order does not depend on the PRNG at all: a room that has *not*
    // consumed the same draws orders the same candidates differently.
    let mut untouched = room_for(
        load(FIXTURE).unwrap(),
        RoomOptions {
            hint_cost: 0,
            ..Default::default()
        },
    );
    assert_ne!(
        next_hints(&mut untouched, 52),
        from_original,
        "hint order does not track the PRNG, so this test proves nothing"
    );
}

#[test]
fn encoding_is_deterministic() {
    if skip_without(FIXTURE) {
        return;
    }
    // Same state, same bytes — every map is sorted before it is written. This
    // is what lets a save be diffed, and what keeps the round-trip tests from
    // depending on hash iteration order.
    let (room, _, _, _) = played_room();
    let snapshot = room.snapshot();
    assert_eq!(snapshot.encode(false), snapshot.encode(false));

    let reloaded = reload(&snapshot.encode(false));
    assert_eq!(
        reloaded.snapshot().encode(false),
        snapshot.encode(false),
        "a save, restored and re-saved, must reproduce itself"
    );
}

#[test]
fn compression_is_transparent() {
    if skip_without(FIXTURE) {
        return;
    }
    let (room, _, _, _) = played_room();
    let snapshot = room.snapshot();
    let raw = snapshot.encode(false);
    let packed = snapshot.encode(true);
    assert!(packed.len() < raw.len(), "compression should shrink a save");
    assert_eq!(
        reload(&packed).snapshot().encode(false),
        raw,
        "the two encodings must restore to the same room"
    );
}

#[test]
fn a_save_from_another_seed_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (room, _, _, _) = played_room();
    let mut snapshot = room.snapshot();
    snapshot.seed_name = "some other seed".to_string();

    let data = load(FIXTURE).unwrap();
    let mut fresh = room_for(data, RoomOptions::default());
    // Loading one seed's checks against another's location table would present
    // as corruption rather than as an error, so this must refuse rather than
    // do its best.
    assert!(matches!(
        fresh.restore(snapshot),
        Err(SaveError::WrongSeed { .. })
    ));
}

#[test]
fn options_survive_a_restart() {
    if skip_without(FIXTURE) {
        return;
    }
    // `/option` changes these while the room is live, so they are state, not
    // configuration, and the reference saves them for the same reason.
    let data = load(FIXTURE).unwrap();
    let room = room_for(
        data,
        RoomOptions {
            hint_cost: 42,
            location_check_points: 7,
            release_mode: pahoa_proto::Permission::AutoEnabled,
            collect_mode: pahoa_proto::Permission::Goal,
            remaining_mode: pahoa_proto::Permission::Disabled,
            item_cheat: false,
            // An empty password is not the same as no password.
            password: Some(String::new()),
            server_password: Some("hunter2".to_string()),
            ..Default::default()
        },
    );

    let restored = reload(&room.snapshot().encode(true));
    let o = &restored.options;
    assert_eq!(o.hint_cost, 42);
    assert_eq!(o.location_check_points, 7);
    assert_eq!(o.release_mode, pahoa_proto::Permission::AutoEnabled);
    assert_eq!(o.collect_mode, pahoa_proto::Permission::Goal);
    assert_eq!(o.remaining_mode, pahoa_proto::Permission::Disabled);
    assert!(!o.item_cheat);
    assert_eq!(o.password.as_deref(), Some(""));
    assert_eq!(o.server_password.as_deref(), Some("hunter2"));
}

#[test]
fn hint_statuses_and_entrances_round_trip() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());
    let hints = vec![
        Hint {
            receiving_player: 1,
            finding_player: 2,
            location: 4_000_000,
            item: -5,
            found: true,
            entrance: "Lost Woods".to_string(),
            item_flags: 0b101,
            status: HintStatus::Found,
        },
        Hint {
            receiving_player: 3,
            finding_player: 2,
            location: -1,
            item: 77,
            found: false,
            // Repeated on purpose: the entrance table must not confuse two
            // hints that share a string.
            entrance: "Lost Woods".to_string(),
            item_flags: 0,
            status: HintStatus::Avoid,
        },
        Hint {
            receiving_player: 3,
            finding_player: 4,
            location: 12,
            item: 13,
            found: false,
            entrance: String::new(),
            item_flags: 0,
            status: HintStatus::Unspecified,
        },
    ];
    room.set_hints((0, 1), hints.clone());

    let restored = reload(&room.snapshot().encode(false));
    assert_eq!(restored.hints_for((0, 1)), hints.as_slice());
}

#[test]
fn a_corrupt_save_is_refused_rather_than_half_loaded() {
    if skip_without(FIXTURE) {
        return;
    }
    let (room, _, _, _) = played_room();
    let good = room.snapshot().encode(false);

    assert!(matches!(
        Snapshot::decode(b"not a save at all"),
        Err(SaveError::BadMagic)
    ));
    assert!(matches!(Snapshot::decode(&[]), Err(SaveError::BadMagic)));

    // Truncation: the length and checksum in the header catch it even when the
    // body happens to parse.
    assert!(Snapshot::decode(&good[..good.len() / 2]).is_err());

    // A flipped bit deep in the body. Byte 200 is well past the header, so this
    // is the checksum's job rather than the parser's.
    let mut flipped = good.clone();
    flipped[200] ^= 0xff;
    assert!(
        matches!(Snapshot::decode(&flipped), Err(SaveError::Checksum)),
        "a corrupted body must be refused"
    );

    let mut newer = good.clone();
    newer[8] = FORMAT_VERSION + 1;
    assert!(matches!(
        Snapshot::decode(&newer),
        Err(SaveError::TooNew { .. })
    ));
}
