//! Location checks, item distribution, and the item stream.

mod common;

use common::*;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

fn checks(locations: Vec<i64>) -> ClientPacket {
    ClientPacket::LocationChecks(cmd::LocationChecks { locations })
}

fn received(sink: &Recorder, conn: ConnId) -> Vec<&pahoa_proto::server::ReceivedItems> {
    sink.packets_for(conn)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::ReceivedItems(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn room_update(sink: &Recorder, conn: ConnId) -> Vec<&pahoa_proto::server::RoomUpdate> {
    sink.packets_for(conn)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::RoomUpdate(r) => Some(&**r),
            _ => None,
        })
        .collect()
}

/// A room plus a joined client on the first player slot.
fn setup() -> Option<(Room, ConnId, u32)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);
    Some((room, conn, slot))
}

#[test]
fn checking_a_location_reports_it_back() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let first = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(conn, checks(vec![first]), &mut sink);

    let updates = room_update(&sink, conn);
    assert_eq!(updates.len(), 1);
    // Only the new check is listed, not the whole set.
    assert_eq!(updates[0].checked_locations.as_deref(), Some(&[first][..]));
    assert!(sink.dirty, "a check must mark the room for saving");
}

#[test]
fn rechecking_the_same_location_is_a_no_op() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let first = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(conn, checks(vec![first]), &mut sink);
    sink.clear();

    room.handle(conn, checks(vec![first]), &mut sink);
    assert!(
        sink.events.is_empty(),
        "duplicate check should produce nothing"
    );
}

#[test]
fn unknown_location_ids_are_dropped_silently() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    let mut sink = Recorder::default();

    // Clients legitimately send ids this multidata has never heard of.
    room.handle(conn, checks(vec![-999_999, 987_654_321]), &mut sink);
    assert!(
        sink.events.is_empty(),
        "unknown ids must not error or broadcast"
    );
}

#[test]
fn duplicate_ids_within_one_packet_are_collapsed() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let first = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(conn, checks(vec![first, first, first]), &mut sink);

    let updates = room_update(&sink, conn);
    assert_eq!(updates[0].checked_locations.as_deref(), Some(&[first][..]));
}

#[test]
fn an_item_reaches_the_slot_it_is_destined_for() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();

    // Find a location in one player's world holding an item for a *different*
    // player, so the cross-slot path is what gets exercised.
    let mut sender = None;
    for (slot, _) in data.player_slots() {
        if let Some(entry) = data
            .locations
            .for_slot(*slot)
            .iter()
            .find(|e| e.receiver != *slot)
        {
            sender = Some((*slot, entry.location, entry.receiver, entry.item));
            break;
        }
    }
    let Some((sender_slot, location, receiver_slot, item_id)) = sender else {
        eprintln!("SKIP: fixture has no cross-slot item placement");
        return;
    };

    let sender_info = &data.slot_info[&sender_slot];
    let receiver_info = &data.slot_info[&receiver_slot];
    let (sender_name, sender_game) = (sender_info.name.clone(), sender_info.game.clone());
    let (receiver_name, receiver_game) = (receiver_info.name.clone(), receiver_info.game.clone());

    let mut room = room_for(data, RoomOptions::default());
    let sender_conn = join(&mut room, 1, &sender_name, &sender_game, 0b111);
    let receiver_conn = join(&mut room, 2, &receiver_name, &receiver_game, 0b111);

    let mut sink = Recorder::default();
    room.handle(sender_conn, checks(vec![location]), &mut sink);

    let got = received(&sink, receiver_conn);
    assert_eq!(
        got.len(),
        1,
        "receiver should get exactly one ReceivedItems"
    );
    assert_eq!(got[0].items.len(), 1);
    assert_eq!(got[0].items[0].item, item_id);
    assert_eq!(got[0].items[0].location, location);
    // `player` is the *sending* slot everywhere except LocationInfo.
    assert_eq!(got[0].items[0].player, sender_slot);
}

#[test]
fn the_item_feed_is_broadcast_to_everyone() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let first = room.multidata().locations.for_slot(slot)[0].location;

    let mut sink = Recorder::default();
    room.handle(conn, checks(vec![first]), &mut sink);

    let feed: Vec<_> = sink
        .broadcasts()
        .flat_map(|(_, msgs)| msgs.iter())
        .filter(|p| matches!(p, ServerPacket::PrintJSON(_)))
        .collect();
    assert_eq!(feed.len(), 1, "one item send, one feed line");
}

#[test]
fn sync_resends_the_whole_inventory_from_index_zero() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let locs: Vec<i64> = room
        .multidata()
        .locations
        .for_slot(slot)
        .iter()
        .take(5)
        .map(|e| e.location)
        .collect();

    let mut sink = Recorder::default();
    room.handle(conn, checks(locs), &mut sink);
    sink.clear();

    room.handle(conn, ClientPacket::Sync, &mut sink);
    let got = received(&sink, conn);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].index, 0, "Sync always restarts the stream");
}

#[test]
fn a_client_with_items_handling_zero_receives_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    // 0b000: never send ReceivedItems at all.
    let conn = join(&mut room, 1, &name, &game, 0b000);

    let locs: Vec<i64> = room
        .multidata()
        .locations
        .for_slot(slot)
        .iter()
        .take(20)
        .map(|e| e.location)
        .collect();

    let mut sink = Recorder::default();
    room.handle(conn, checks(locs), &mut sink);
    room.handle(conn, ClientPacket::Sync, &mut sink);

    assert!(
        received(&sink, conn).is_empty(),
        "no items should ever be sent"
    );
}

#[test]
fn a_tracker_may_not_check_locations() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, _) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    room.handle(
        conn,
        ClientPacket::Connect(Box::new(cmd::Connect {
            password: None,
            game: None,
            name: name.clone(),
            uuid: serde_json::json!(null),
            version: pahoa_proto::types::Version::new(0, 6, 8),
            items_handling: 0,
            tags: vec!["Tracker".to_string()],
            slot_data: false,
        })),
        &mut sink,
    );
    sink.clear();

    let first = room.multidata().locations.for_slot(slot)[0].location;
    room.handle(conn, checks(vec![first]), &mut sink);

    let invalid: Vec<_> = sink
        .packets_for(conn)
        .into_iter()
        .filter(|p| matches!(p, ServerPacket::InvalidPacket(_)))
        .collect();
    assert_eq!(invalid.len(), 1, "should be refused with InvalidPacket");
}

#[test]
fn goal_status_is_irreversible() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let mut sink = Recorder::default();

    room.handle(
        conn,
        ClientPacket::StatusUpdate(cmd::StatusUpdate { status: 30 }),
        &mut sink,
    );
    assert_eq!(room.status((0, slot)), pahoa_proto::ClientStatus::Goal);

    // Trying to walk it back must not take effect.
    room.handle(
        conn,
        ClientPacket::StatusUpdate(cmd::StatusUpdate { status: 20 }),
        &mut sink,
    );
    assert_eq!(room.status((0, slot)), pahoa_proto::ClientStatus::Goal);
}

#[test]
fn spectators_and_groups_start_already_goaled() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let non_player: Vec<u32> = data
        .slot_info
        .iter()
        .filter(|(_, i)| i.slot_type != pahoa_multidata::SlotType::Player)
        .map(|(s, _)| *s)
        .collect();
    if non_player.is_empty() {
        eprintln!("SKIP: fixture has no spectator or group slots");
        return;
    }

    let room = room_for(data, RoomOptions::default());
    for slot in non_player {
        assert_eq!(
            room.status((0, slot)),
            pahoa_proto::ClientStatus::Goal,
            "slot {slot} should be goal-complete at load"
        );
    }
}
