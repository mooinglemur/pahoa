//! Hint notification: who hears about a hint, in what order, and what a
//! tracker subscribed to `_read_hints_*` sees.
//!
//! The storage and ordering rules are unit-tested inside `pahoa-room::hints`;
//! this covers the wiring — the fan-out to finder and receiver, the
//! `RoomUpdate` that follows a banked hint, the `SetReply` push to
//! subscribers, and the recheck that turns a hint "found" when its location is
//! checked.

mod common;

use common::*;
use pahoa_multidata::{Hint, HintStatus};
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};
use serde_json::Value;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn print_json<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
) -> Vec<&'a pahoa_proto::server::PrintJson> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::PrintJSON(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn room_updates<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
) -> Vec<&'a pahoa_proto::server::RoomUpdate> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::RoomUpdate(r) => Some(&**r),
            _ => None,
        })
        .collect()
}

fn set_replies<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
) -> Vec<&'a serde_json::Map<String, Value>> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::Echo(m) if m.get("cmd") == Some(&Value::from("SetReply")) => Some(m),
            _ => None,
        })
        .collect()
}

/// Hints a slot holds for one location.
///
/// The fixture ships precollected hints for several slots, so asserting on the
/// length of a whole list would measure the seed rather than the test.
fn stored_at(room: &Room, slot: u32, location: i64) -> Vec<&Hint> {
    room.hints_for((0, slot))
        .iter()
        .filter(|h| h.location == location)
        .collect()
}

/// A location in `slot`'s world that the seed has not already hinted.
fn unhinted_location(room: &Room, slot: u32) -> i64 {
    room.multidata()
        .locations
        .for_slot(slot)
        .iter()
        .map(|e| e.location)
        .find(|loc| stored_at(room, slot, *loc).is_empty())
        .expect("some location is unhinted")
}

fn hint(receiving: u32, finding: u32, location: i64, item: i64) -> Hint {
    Hint {
        receiving_player: receiving,
        finding_player: finding,
        location,
        item,
        found: false,
        entrance: String::new(),
        item_flags: 0,
        status: HintStatus::Priority,
    }
}

/// A joined connection and the slot it holds.
type Player = (ConnId, u32);

/// Two joined player slots, plus the room.
fn two_players() -> Option<(Room, Player, Player)> {
    let data = load(FIXTURE)?;
    let mut players = data.player_slots();
    let (a_slot, a_info) = players.next()?;
    let (b_slot, b_info) = players.next()?;
    let (a_slot, a_name, a_game) = (*a_slot, a_info.name.clone(), a_info.game.clone());
    let (b_slot, b_name, b_game) = (*b_slot, b_info.name.clone(), b_info.game.clone());
    drop(players);

    let mut room = room_for(data, RoomOptions::default());
    let a = join(&mut room, 1, &a_name, &a_game, 0b111);
    let b = join(&mut room, 2, &b_name, &b_game, 0b111);
    Some((room, (a, a_slot), (b, b_slot)))
}

#[test]
fn a_hint_reaches_both_the_finder_and_the_receiver() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), (b, b_slot)) = two_players().unwrap();

    let mut sink = Recorder::default();
    // B's item sits in A's world: both of them care.
    room.notify_hints(
        0,
        vec![hint(b_slot, a_slot, 1234, 99)],
        false,
        false,
        None,
        &mut sink,
    );

    for (who, conn) in [("finder", a), ("receiver", b)] {
        let msgs = print_json(&sink, conn, &room);
        assert_eq!(msgs.len(), 1, "{who} should get exactly one hint line");
        assert_eq!(
            msgs[0].print_type,
            Some(pahoa_proto::server::PrintJsonType::Hint)
        );
        assert_eq!(msgs[0].found, Some(false));
    }

    // Both slots banked a hint, so both are told their points moved.
    for conn in [a, b] {
        let updates = room_updates(&sink, conn, &room);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].hint_points.is_some());
        assert!(updates[0].checked_locations.is_none());
    }

    assert_eq!(stored_at(&room, a_slot, 1234).len(), 1);
    assert_eq!(stored_at(&room, b_slot, 1234).len(), 1);
    assert!(sink.dirty);
}

#[test]
fn a_local_hint_is_announced_once_not_twice() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), _) = two_players().unwrap();

    let mut sink = Recorder::default();
    room.notify_hints(
        0,
        vec![hint(a_slot, a_slot, 1234, 99)],
        false,
        false,
        None,
        &mut sink,
    );

    assert_eq!(
        print_json(&sink, a, &room).len(),
        1,
        "finder and receiver are the same slot, so the hint is sent once"
    );
}

#[test]
fn only_new_drops_hints_the_finder_already_holds() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), (_, b_slot)) = two_players().unwrap();
    let h = hint(b_slot, a_slot, 1234, 99);

    let mut sink = Recorder::default();
    room.notify_hints(0, vec![h.clone()], false, false, None, &mut sink);
    sink.clear();

    // Same hint again with only_new: silence.
    room.notify_hints(0, vec![h.clone()], true, false, None, &mut sink);
    assert!(print_json(&sink, a, &room).is_empty());
    assert!(!sink.dirty);

    // Without only_new it is re-announced, but not stored twice.
    room.notify_hints(0, vec![h], false, false, None, &mut sink);
    assert_eq!(print_json(&sink, a, &room).len(), 1);
    assert_eq!(stored_at(&room, a_slot, 1234).len(), 1);
    assert!(
        room_updates(&sink, a, &room).is_empty(),
        "nothing was banked, so no points changed"
    );
}

#[test]
fn a_found_hint_is_only_remembered_when_the_caller_asks() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, (_, b_slot)) = two_players().unwrap();
    let a_slot = room
        .multidata()
        .player_slots()
        .next()
        .map(|(s, _)| *s)
        .unwrap();

    let mut found = hint(b_slot, a_slot, 1234, 99);
    found.found = true;
    found.status = HintStatus::Found;

    // `!hint` semantics: announced, not banked.
    let mut sink = Recorder::default();
    room.notify_hints(0, vec![found.clone()], false, false, None, &mut sink);
    assert!(stored_at(&room, a_slot, 1234).is_empty());

    // Scout semantics: banked even though it is already found.
    room.notify_hints(0, vec![found], false, true, None, &mut sink);
    assert_eq!(stored_at(&room, a_slot, 1234).len(), 1);
}

#[test]
fn recipients_limits_delivery_without_limiting_storage() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), (b, b_slot)) = two_players().unwrap();

    let mut sink = Recorder::default();
    room.notify_hints(
        0,
        vec![hint(b_slot, a_slot, 1234, 99)],
        false,
        false,
        Some(&[a_slot]),
        &mut sink,
    );

    assert_eq!(print_json(&sink, a, &room).len(), 1);
    assert!(
        print_json(&sink, b, &room).is_empty(),
        "B was not in the recipient list"
    );
    // Storage is unaffected: B still owns the hint and can read it back.
    assert_eq!(stored_at(&room, b_slot, 1234).len(), 1);
}

#[test]
fn subscribers_are_pushed_the_whole_hint_list() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), (_, b_slot)) = two_players().unwrap();
    let key = format!("_read_hints_0_{a_slot}");

    let mut sink = Recorder::default();
    room.handle(
        a,
        ClientPacket::SetNotify(cmd::SetNotify {
            keys: vec![key.clone()],
        }),
        &mut sink,
    );
    sink.clear();

    room.notify_hints(
        0,
        vec![hint(b_slot, a_slot, 1234, 99)],
        false,
        false,
        None,
        &mut sink,
    );

    let replies = set_replies(&sink, a, &room);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].get("key"), Some(&Value::from(key)));
    // A fresh dict, not an echo of a client request: no original_value, no slot.
    let keys: Vec<&str> = replies[0].keys().map(String::as_str).collect();
    assert_eq!(keys, ["cmd", "key", "value"]);

    let hints = replies[0]["value"].as_array().unwrap();
    let ours = hints
        .iter()
        .find(|h| h["location"] == 1234)
        .expect("the new hint is in the pushed list");
    assert_eq!(ours["class"], Value::from("Hint"));
    assert_eq!(ours["finding_player"], Value::from(a_slot));
    assert_eq!(ours["status"], Value::from(30));
    assert_eq!(
        hints.len(),
        room.hints_for((0, a_slot)).len(),
        "the push carries the whole list, not just what changed"
    );
}

#[test]
fn reading_the_hints_key_returns_the_slots_hints() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), (_, b_slot)) = two_players().unwrap();

    let mut sink = Recorder::default();
    room.notify_hints(
        0,
        vec![hint(b_slot, a_slot, 1234, 99)],
        false,
        false,
        None,
        &mut sink,
    );
    sink.clear();

    let key = format!("_read_hints_0_{a_slot}");
    let mut raw = serde_json::Map::new();
    raw.insert("cmd".into(), Value::from("Get"));
    raw.insert("keys".into(), serde_json::json!([key]));
    room.handle(
        a,
        ClientPacket::Get(
            cmd::Get {
                keys: vec![key.clone()],
            },
            raw,
        ),
        &mut sink,
    );

    let retrieved = sink
        .packets_for(a, &room)
        .into_iter()
        .find_map(|p| match p {
            ServerPacket::Echo(m) if m.get("cmd") == Some(&Value::from("Retrieved")) => Some(m),
            _ => None,
        })
        .expect("a Retrieved reply");
    let listed = retrieved["keys"][&key].as_array().unwrap();
    assert_eq!(listed.len(), room.hints_for((0, a_slot)).len());
    assert!(listed.iter().any(|h| h["location"] == 1234));
}

#[test]
fn checking_a_hinted_location_marks_the_hint_found_and_tells_subscribers() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, (a, a_slot), (b, b_slot)) = two_players().unwrap();
    let location = unhinted_location(&room, a_slot);

    // Both slots subscribe to A's hint list so we can see the push.
    let key = format!("_read_hints_0_{a_slot}");
    let mut sink = Recorder::default();
    for conn in [a, b] {
        room.handle(
            conn,
            ClientPacket::SetNotify(cmd::SetNotify {
                keys: vec![key.clone()],
            }),
            &mut sink,
        );
    }
    room.notify_hints(
        0,
        vec![hint(b_slot, a_slot, location, 99)],
        false,
        false,
        None,
        &mut sink,
    );
    sink.clear();

    room.handle(
        a,
        ClientPacket::LocationChecks(cmd::LocationChecks {
            locations: vec![location],
        }),
        &mut sink,
    );

    let stored = stored_at(&room, a_slot, location);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].found, "checking the location makes it found");
    assert_eq!(stored[0].status, HintStatus::Found);
    // The receiver's copy moves too, or a tracker would show it as unfound.
    assert!(stored_at(&room, b_slot, location)[0].found);

    let replies = set_replies(&sink, a, &room);
    assert_eq!(replies.len(), 1, "subscribers hear about the change once");
    let pushed = replies[0]["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["location"] == location)
        .expect("the rechecked hint is in the push");
    assert_eq!(pushed["found"], Value::from(true));
}

#[test]
fn no_text_clients_get_the_points_update_but_not_the_hint_line() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let quiet = ConnId(9);
    let mut sink = Recorder::default();
    room.on_connect(quiet, &mut sink);
    let ClientPacket::Connect(mut c) = connect(&name, &game, 0b111) else {
        unreachable!()
    };
    c.tags = vec!["NoText".to_string()];
    room.handle(quiet, ClientPacket::Connect(c), &mut sink);
    sink.clear();

    room.notify_hints(
        0,
        vec![hint(slot, slot, 1234, 99)],
        false,
        false,
        None,
        &mut sink,
    );

    assert!(
        print_json(&sink, quiet, &room).is_empty(),
        "NoText suppresses the hint chat line"
    );
    assert_eq!(
        room_updates(&sink, quiet, &room).len(),
        1,
        "but not the hint_points update, which is state not chat"
    );
}

#[test]
fn seeded_hints_from_the_multidata_are_loaded_at_startup() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let seeded: usize = data.precollected_hints.values().map(Vec::len).sum();
    let room = room_for(data.clone(), RoomOptions::default());

    let loaded: usize = data
        .precollected_hints
        .keys()
        .map(|slot| room.hints_for((0, *slot)).len())
        .sum();
    assert_eq!(loaded, seeded);
}
