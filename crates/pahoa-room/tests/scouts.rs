//! `LocationScouts`, `CreateHints` and `UpdateHint`.
//!
//! These are the three ways a client creates or changes a hint without going
//! through chat, and between them they carry most of the protocol's sharp
//! edges: the inverted `player` field in `LocationInfo`, the three-way
//! `create_as_hint` switch, the "you may only editorialize about your own
//! items" rule, and the handlers that raise and drop the socket instead of
//! answering `InvalidPacket`.

mod common;

use common::*;
use pahoa_multidata::HintStatus;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{CloseReason, ConnId, Event, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

fn location_info<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
) -> Vec<&'a pahoa_proto::server::LocationInfo> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::LocationInfo(l) => Some(l),
            _ => None,
        })
        .collect()
}

fn invalid<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
) -> Vec<&'a pahoa_proto::server::InvalidPacket> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::InvalidPacket(i) => Some(i),
            _ => None,
        })
        .collect()
}

fn hint_lines(sink: &Recorder, conn: ConnId, room: &Room) -> usize {
    sink.packets_for(conn, room)
        .into_iter()
        .filter(|p| {
            matches!(p, ServerPacket::PrintJSON(m)
                if m.print_type == Some(pahoa_proto::server::PrintJsonType::Hint))
        })
        .count()
}

fn closed(sink: &Recorder, conn: ConnId) -> bool {
    sink.events.iter().any(|e| {
        matches!(e, Event::Close { conn: c, reason: CloseReason::ProtocolError(_) } if *c == conn)
    })
}

fn scouts(locations: Vec<i64>, create_as_hint: i64) -> ClientPacket {
    ClientPacket::LocationScouts(cmd::LocationScouts {
        locations,
        create_as_hint,
    })
}

/// A room, a joined client, and its slot.
fn setup() -> Option<(Room, ConnId, u32)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);
    Some((room, conn, slot))
}

/// A location in `slot`'s world holding an item for someone else, if any.
fn remote_placement(room: &Room, slot: u32) -> Option<(i64, u32, i64)> {
    room.multidata()
        .locations
        .for_slot(slot)
        .iter()
        .find(|e| e.receiver != slot)
        .map(|e| (e.location, e.receiver, e.item))
}

/// A location in `slot`'s world holding `slot`'s own item.
///
/// The permission rules turn on who receives, not who finds, so a test about
/// what a player may say about their *own* item has to pick one deliberately.
fn local_placement(room: &Room, slot: u32) -> Option<i64> {
    room.multidata()
        .locations
        .for_slot(slot)
        .iter()
        .find(|e| e.receiver == slot)
        .map(|e| e.location)
}

#[test]
fn scouting_answers_with_the_item_at_each_location() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let entries: Vec<_> = room.multidata().locations.for_slot(slot)[..3].to_vec();

    let mut sink = Recorder::default();
    room.handle(
        conn,
        scouts(entries.iter().map(|e| e.location).collect(), 0),
        &mut sink,
    );

    let info = location_info(&sink, conn, &room);
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].locations.len(), 3);
    for (got, want) in info[0].locations.iter().zip(&entries) {
        assert_eq!(got.item, want.item);
        assert_eq!(got.location, want.location);
        assert_eq!(got.flags, want.flags);
        // The inversion: `player` is who *receives*, not who owns the location.
        assert_eq!(got.player, want.receiver);
    }
    // create_as_hint 0 banks nothing and says nothing.
    assert_eq!(hint_lines(&sink, conn, &room), 0);
    assert!(!sink.dirty);
}

#[test]
fn scouting_with_create_as_hint_banks_and_announces() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let location = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![location], 1), &mut sink);

    assert_eq!(hint_lines(&sink, conn, &room), 1);
    assert!(sink.dirty);
    let stored = room
        .hints_for((0, slot))
        .iter()
        .find(|h| h.location == location)
        .expect("the scout banked a hint");
    // Scouts create "unspecified" hints: the player has not said anything about
    // whether they want the item.
    assert_eq!(stored.status, HintStatus::Unspecified);
}

#[test]
fn create_as_hint_two_announces_only_what_is_new() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let locations: Vec<i64> = room.multidata().locations.for_slot(slot)[..2]
        .iter()
        .map(|e| e.location)
        .collect();

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![locations[0]], 2), &mut sink);
    assert_eq!(hint_lines(&sink, conn, &room), 1);

    // Re-scouting the first plus one new one announces only the new one, but
    // still answers with both locations.
    sink.clear();
    room.handle(conn, scouts(locations.clone(), 2), &mut sink);
    assert_eq!(hint_lines(&sink, conn, &room), 1);
    assert_eq!(location_info(&sink, conn, &room)[0].locations.len(), 2);

    // Mode 1 re-announces both, because it does not filter.
    sink.clear();
    room.handle(conn, scouts(locations, 1), &mut sink);
    assert_eq!(hint_lines(&sink, conn, &room), 2);
}

#[test]
fn a_scout_remembers_a_hint_even_for_a_location_already_checked() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let location = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::LocationChecks(cmd::LocationChecks {
            locations: vec![location],
        }),
        &mut sink,
    );
    sink.clear();

    room.handle(conn, scouts(vec![location], 1), &mut sink);
    let stored = room
        .hints_for((0, slot))
        .iter()
        .find(|h| h.location == location)
        .expect("persist_even_if_found keeps this one");
    assert!(stored.found);
    assert_eq!(stored.status, HintStatus::Found);
}

#[test]
fn scouting_an_unknown_location_drops_the_socket() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![i64::MAX], 0), &mut sink);

    // The reference indexes its location table and raises, which unwinds to the
    // socket loop. Faithful means disconnecting, not answering.
    assert!(closed(&sink, conn));
    assert!(location_info(&sink, conn, &room).is_empty());
}

#[test]
fn create_hints_rejects_an_empty_location_list() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![],
            player: None,
            status: None,
        }),
        &mut sink,
    );

    let refused = invalid(&sink, conn, &room);
    assert_eq!(refused.len(), 1);
    assert!(refused[0].text.contains("No locations specified"));
}

#[test]
fn create_hints_rejects_an_unknown_status() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let location = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![location],
            player: None,
            status: Some(35),
        }),
        &mut sink,
    );

    let refused = invalid(&sink, conn, &room);
    assert_eq!(refused.len(), 1);
    assert!(refused[0].text.starts_with("Unknown Status"));
}

#[test]
fn create_hints_lets_a_slot_prioritize_inside_its_own_world() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let Some(location) = local_placement(&room, slot) else {
        eprintln!("SKIP: no local placement in this slot");
        return;
    };

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![location],
            player: None,
            status: Some(HintStatus::Avoid as i64),
        }),
        &mut sink,
    );

    assert!(invalid(&sink, conn, &room).is_empty());
    let stored = room
        .hints_for((0, slot))
        .iter()
        .find(|h| h.location == location)
        .expect("hint created");
    assert_eq!(stored.status, HintStatus::Avoid);
    assert!(sink.dirty);
}

#[test]
fn create_hints_refuses_to_editorialize_about_someone_elses_item() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let Some((location, _receiver, _)) = remote_placement(&room, slot) else {
        eprintln!("SKIP: no remote placement in this slot");
        return;
    };

    // The location is ours, but the item is not: only "unspecified" is allowed.
    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![location],
            player: None,
            status: Some(HintStatus::Priority as i64),
        }),
        &mut sink,
    );

    let refused = invalid(&sink, conn, &room);
    assert_eq!(refused.len(), 1);
    assert!(refused[0].text.contains("unspecified"), "{:?}", refused[0]);

    // Unspecified is fine, because it says nothing.
    sink.clear();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![location],
            player: None,
            status: None,
        }),
        &mut sink,
    );
    assert!(invalid(&sink, conn, &room).is_empty());
}

#[test]
fn create_hints_refuses_an_off_world_location_that_does_not_exist() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let other = room
        .multidata()
        .player_slots()
        .map(|(s, _)| *s)
        .find(|s| *s != slot)
        .expect("a second slot");

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![i64::MAX],
            player: Some(other),
            status: None,
        }),
        &mut sink,
    );

    // Off-world gets a message rather than a disconnect: the client is being
    // told not to fish for other people's locations.
    let refused = invalid(&sink, conn, &room);
    assert_eq!(refused.len(), 1);
    assert!(refused[0].text.contains("off-world"));
    assert!(!closed(&sink, conn));
}

#[test]
fn create_hints_drops_the_socket_on_an_unknown_own_location() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::CreateHints(cmd::CreateHints {
            locations: vec![i64::MAX],
            player: None,
            status: None,
        }),
        &mut sink,
    );

    assert!(closed(&sink, conn));
}

#[test]
fn update_hint_changes_the_status_for_every_slot_holding_a_copy() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let Some((location, receiver, _)) = remote_placement(&room, slot) else {
        eprintln!("SKIP: no remote placement in this slot");
        return;
    };

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![location], 1), &mut sink);
    sink.clear();

    // The receiver owns the priority decision, so drive it from their side.
    let receiver_conn = {
        let info = &room.multidata().slot_info[&receiver];
        let (name, game) = (info.name.clone(), info.game.clone());
        join(&mut room, 2, &name, &game, 0b111)
    };

    room.handle(
        receiver_conn,
        ClientPacket::UpdateHint(cmd::UpdateHint {
            player: slot,
            location,
            status: Some(HintStatus::Priority as i64),
        }),
        &mut sink,
    );

    assert!(invalid(&sink, receiver_conn, &room).is_empty());
    for holder in [slot, receiver] {
        let stored = room
            .hints_for((0, holder))
            .iter()
            .find(|h| h.location == location && h.finding_player == slot)
            .expect("both slots hold a copy");
        assert_eq!(
            stored.status,
            HintStatus::Priority,
            "slot {holder}'s copy should move too"
        );
    }
    assert!(sink.dirty);
}

#[test]
fn only_the_receiving_player_may_reprioritize() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let Some((location, _receiver, _)) = remote_placement(&room, slot) else {
        eprintln!("SKIP: no remote placement in this slot");
        return;
    };

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![location], 1), &mut sink);
    sink.clear();

    // The finder owns the location but not the item.
    room.handle(
        conn,
        ClientPacket::UpdateHint(cmd::UpdateHint {
            player: slot,
            location,
            status: Some(HintStatus::Priority as i64),
        }),
        &mut sink,
    );

    let refused = invalid(&sink, conn, &room);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].text, "UpdateHint: No Permission");
}

#[test]
fn update_hint_refuses_to_set_found_by_hand() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let Some(location) = local_placement(&room, slot) else {
        eprintln!("SKIP: no local placement in this slot");
        return;
    };

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![location], 1), &mut sink);
    sink.clear();

    room.handle(
        conn,
        ClientPacket::UpdateHint(cmd::UpdateHint {
            player: slot,
            location,
            status: Some(HintStatus::Found as i64),
        }),
        &mut sink,
    );

    let refused = invalid(&sink, conn, &room);
    assert_eq!(refused.len(), 1);
    assert!(refused[0].text.contains("HINT_FOUND"));

    // An unknown status is a different message.
    sink.clear();
    room.handle(
        conn,
        ClientPacket::UpdateHint(cmd::UpdateHint {
            player: slot,
            location,
            status: Some(99),
        }),
        &mut sink,
    );
    assert_eq!(
        invalid(&sink, conn, &room)[0].text,
        "UpdateHint: Invalid Status"
    );
}

#[test]
fn update_hint_ignores_a_hint_that_does_not_exist() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::UpdateHint(cmd::UpdateHint {
            player: slot,
            location: i64::MAX,
            status: Some(HintStatus::Priority as i64),
        }),
        &mut sink,
    );

    // Silently ignored: a client may be working from a stale hint list.
    assert!(invalid(&sink, conn, &room).is_empty());
    assert!(!closed(&sink, conn));
    assert!(!sink.dirty);
}

#[test]
fn a_null_status_leaves_the_hint_alone() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let Some(location) = local_placement(&room, slot) else {
        eprintln!("SKIP: no local placement in this slot");
        return;
    };

    let mut sink = Recorder::default();
    room.handle(conn, scouts(vec![location], 1), &mut sink);
    sink.clear();

    room.handle(
        conn,
        ClientPacket::UpdateHint(cmd::UpdateHint {
            player: slot,
            location,
            status: None,
        }),
        &mut sink,
    );

    assert!(invalid(&sink, conn, &room).is_empty());
    assert!(!sink.dirty);
    let stored = room
        .hints_for((0, slot))
        .iter()
        .find(|h| h.location == location)
        .unwrap();
    assert_eq!(stored.status, HintStatus::Unspecified);
}
