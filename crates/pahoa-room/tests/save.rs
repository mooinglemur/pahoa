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

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

/// A room with a bit of everything in it: checks, items owed, hints, aliases,
/// datastorage, a spent hint budget.
fn played_room() -> (Room, u32, String, String) {
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = richest_player(&data);
    let mut room = room_for(
        data.clone(),
        RoomOptions {
            hint_cost: 0,
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    let hintable = most_owed_item(&room, slot).unwrap_or_default();
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
            text: format!("!hint {hintable}"),
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
    let (mut room, slot, name, game) = played_room();
    let mut restored = reload(&room.snapshot().encode(false));

    // Read the *announcement* order rather than the store: these placements are
    // already banked under their finders from the hint in `played_room`, and
    // `notify_hints` will not bank a second copy. The order they are announced
    // in is what the shuffle decided, and is what a player sees.
    let hintable = most_owed_item(&room, slot).expect("an item the slot is owed");
    let next_hints = |room: &mut Room, id: u64| {
        let conn = join(room, id, &name, &game, 0b111);
        let mut sink = Recorder::default();
        room.handle(
            conn,
            ClientPacket::Say(cmd::Say {
                text: format!("!hint {hintable}"),
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
    // configuration, and the reference saves them for the same reason. The
    // secrets are the exception and are covered by the test below.
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
}

/// Secrets are configuration, not state: the environment is authoritative on
/// every start.
///
/// This is the regression test for a real bug. Passwords used to be the first
/// two fields of the saved options, and `Room::restore` assigns them wholesale
/// — so the value on disk won, a rotated password reverted on the next restart,
/// and the configured value was never actually in force.
#[test]
fn a_saved_password_never_replaces_the_configured_one() {
    if skip_without(FIXTURE) {
        return;
    }
    // A room started with one set of secrets, and saved.
    let mut before = room_for(
        load(FIXTURE).unwrap(),
        RoomOptions {
            password: Some("original".to_string()),
            server_password: Some("original-admin".to_string()),
            ..Default::default()
        },
    );
    before.options.slot_passwords = Some(std::collections::BTreeMap::from([(
        3,
        "original-slot-3".to_string(),
    )]));
    let bytes = before.snapshot().encode(true);

    // Restarted with a different set. Rotation has to survive the restart,
    // which is the whole point.
    let mut after = room_for(
        load(FIXTURE).unwrap(),
        RoomOptions {
            password: Some("rotated".to_string()),
            server_password: Some("rotated-admin".to_string()),
            ..Default::default()
        },
    );
    after.options.slot_passwords = Some(std::collections::BTreeMap::from([(
        3,
        "rotated-slot-3".to_string(),
    )]));
    after
        .restore(Snapshot::decode(&bytes).expect("save decodes"))
        .expect("save restores");

    assert_eq!(after.options.password.as_deref(), Some("rotated"));
    assert_eq!(
        after.options.server_password.as_deref(),
        Some("rotated-admin")
    );
    assert_eq!(
        after
            .options
            .slot_passwords
            .as_ref()
            .and_then(|p| p.get(&3))
            .map(String::as_str),
        Some("rotated-slot-3")
    );
}

/// An async routinely outlives the process serving it, so the timestamps a
/// tracker reports have to survive a restart. Without this a restarted room
/// says "never connected" for everyone, and an abandoned slot becomes
/// indistinguishable from an active one.
#[test]
fn tracker_timestamps_survive_a_restart() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = richest_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());

    // Connecting stamps the connection timer; checking a location stamps the
    // activity timer.
    let conn = join(&mut room, 1, &name, &game, 0b111);
    let first_location = data.locations.for_slot(slot)[0].location;
    let mut sink = pahoa_room::Recorder::default();
    room.handle(
        conn,
        pahoa_proto::ClientPacket::LocationChecks(pahoa_proto::client::LocationChecks {
            locations: vec![first_location],
        }),
        &mut sink,
    );

    let before = room.tracker_data();
    let seen = before
        .slots
        .iter()
        .find(|s| s.slot == slot)
        .expect("the slot is in the seed");
    assert!(seen.last_connection.is_some(), "should have connected");
    assert!(
        seen.last_activity.is_some(),
        "should have checked something"
    );

    let restored = reload(&room.snapshot().encode(true));
    let after = restored.tracker_data();
    let kept = after
        .slots
        .iter()
        .find(|s| s.slot == slot)
        .expect("the slot is in the seed");

    // Whole seconds, which is all the tracker's RFC 1123 rendering can carry.
    assert_eq!(
        kept.last_connection.map(|t| t as u64),
        seen.last_connection.map(|t| t as u64),
        "the connection timer should have survived"
    );
    assert_eq!(
        kept.last_activity.map(|t| t as u64),
        seen.last_activity.map(|t| t as u64),
        "the activity timer should have survived"
    );

    // A slot that never did anything still reports nothing, rather than a zero
    // that would render as 1970.
    let untouched = after
        .slots
        .iter()
        .find(|s| s.slot != slot)
        .expect("the fixture has more than one slot");
    assert!(untouched.last_connection.is_none());
    assert!(untouched.last_activity.is_none());
}

/// The other direction, and the one that would be silent: a room restarted
/// *without* a password must not have one restored from disk either.
#[test]
fn restoring_into_a_passwordless_room_leaves_it_passwordless() {
    if skip_without(FIXTURE) {
        return;
    }
    let before = room_for(
        load(FIXTURE).unwrap(),
        RoomOptions {
            password: Some("was-set-once".to_string()),
            ..Default::default()
        },
    );
    let bytes = before.snapshot().encode(true);

    let restored = reload(&bytes);
    assert_eq!(restored.options.password, None);
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

/// Header layout, which these tests have to rebuild by hand: magic, version,
/// encoding, body length, body CRC.
const HEADER: usize = 8 + 1 + 1 + 4 + 4;

/// Restamp a body as a given format version, recomputing what the header
/// carries about it. Anything less would produce a file the decoder rejects for
/// the wrong reason.
fn reheader(version: u8, body: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(body);
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(b"PAHOASAV");
    out.push(version);
    out.push(0); // uncompressed
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// **A version-1 save still loads, with no locks.**
///
/// Version 2 appended `locked_slots`, and rooms saved before it must keep
/// working — the absence has to read as "nothing was locked" rather than as a
/// parse failure, so an operator upgrading mid-async notices nothing.
#[test]
fn a_version_one_save_loads_without_locks() {
    if skip_without(FIXTURE) {
        return;
    }
    let (room, ..) = played_room();
    let snapshot = room.snapshot();

    // A version-1 body is this one without its trailing `locked_slots` count,
    // which for a room with no locks is the single byte `0`.
    let v2 = snapshot.encode(false);
    let body = &v2[HEADER..];
    assert_eq!(body[body.len() - 1], 0, "no locks encodes as a zero count");
    let v1 = reheader(1, &body[..body.len() - 1]);

    let decoded = Snapshot::decode(&v1).expect("a version 1 save must still load");
    assert!(
        decoded.locked_slots.is_empty(),
        "a format that predates locks has none, and that is not an error"
    );
    // And the rest survived, so this is a real save rather than a shape that
    // happens to parse.
    assert_eq!(decoded.seed_name, snapshot.seed_name);
    assert_eq!(decoded.name_aliases, snapshot.name_aliases);
    assert_eq!(
        decoded.location_checks.len(),
        snapshot.location_checks.len()
    );
}

/// The trailing field is read on the **version**, not on whether bytes remain.
///
/// A v1 body with the v2 field's bytes appended must not pick them up: were the
/// decoder inferring absence from the data, this would parse as a lock and the
/// format would have no way to ever remove a field again.
#[test]
fn a_version_one_save_ignores_anything_after_its_last_field() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = played_room();
    room.lock_slot((0, slot), true);
    let v2 = room.snapshot().encode(false);
    let body = &v2[HEADER..];

    let decoded = Snapshot::decode(&reheader(1, body)).expect("still a valid v1 body");
    assert!(
        decoded.locked_slots.is_empty(),
        "v1 must not read a field its format does not have"
    );
}

/// Locks round-trip through the encoder like any other per-slot state.
#[test]
fn locked_slots_round_trip() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = played_room();
    room.lock_slot((0, slot), true);
    room.lock_slot((0, slot + 1), true);
    room.lock_slot((0, slot + 1), false);

    let bytes = room.snapshot().encode(true);
    let decoded = Snapshot::decode(&bytes).expect("decodes");
    assert_eq!(decoded.locked_slots, vec![(0, slot)]);
}
