//! Location checks, item distribution, and the item stream.

mod common;

use common::*;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};
use std::sync::Arc;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn checks(locations: Vec<i64>) -> ClientPacket {
    ClientPacket::LocationChecks(cmd::LocationChecks { locations })
}

fn received<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
) -> Vec<&'a pahoa_proto::server::ReceivedItems> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::ReceivedItems(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn room_update<'a>(
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

    let updates = room_update(&sink, conn, &room);
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

    let updates = room_update(&sink, conn, &room);
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

    let got = received(&sink, receiver_conn, &room);
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
    let got = received(&sink, conn, &room);
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
        received(&sink, conn, &room).is_empty(),
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
        .packets_for(conn, &room)
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

/// `Room::last_check_at` — the room-wide "is anyone still playing" timer.
///
/// Distinct from "has any client sent a packet", which chat, `Sync`, `Get` and
/// `StatusUpdate` all reset. An idle reaper wants this one; the reference
/// auto-shuts rooms down on exactly it (`MultiServer.py:2671-2682`).
mod last_check_at {
    use super::*;

    /// `None` rather than an epoch, and the distinction is load-bearing: a room
    /// whose organizer is still getting people connected has this shape, and an
    /// orchestrator must be able to tell it from a check that happened in 1970.
    #[test]
    fn an_unplayed_room_reports_no_check_at_all() {
        if skip_without(FIXTURE) {
            return;
        }
        let (room, _conn, _slot) = setup().unwrap();
        assert_eq!(
            room.last_check_at(),
            None,
            "a room nobody has played must not report a time"
        );
    }

    #[test]
    fn a_check_sets_it_to_the_room_clock() {
        if skip_without(FIXTURE) {
            return;
        }
        let (mut room, conn, slot) = setup().unwrap();
        let first = room.multidata().locations.for_slot(slot)[0].location;

        let mut sink = Recorder::default();
        room.tick(1_700_000_500.0, &mut sink);
        room.handle(conn, checks(vec![first]), &mut sink);

        assert_eq!(room.last_check_at(), Some(1_700_000_500.0));
    }

    /// **The guard that makes this usable as an idle signal at all.**
    ///
    /// A client re-sends its whole location list on every reconnect. If that
    /// counted, a room full of reconnecting-but-idle clients would look active
    /// forever and never reap — which is the failure the room-wide timer exists
    /// to avoid, reintroduced one layer down.
    #[test]
    fn resending_known_checks_does_not_advance_it() {
        if skip_without(FIXTURE) {
            return;
        }
        let (mut room, conn, slot) = setup().unwrap();
        let first = room.multidata().locations.for_slot(slot)[0].location;

        let mut sink = Recorder::default();
        room.tick(1_700_000_500.0, &mut sink);
        room.handle(conn, checks(vec![first]), &mut sink);

        // Much later, the same client says the same thing again.
        room.tick(1_700_090_000.0, &mut sink);
        room.handle(conn, checks(vec![first]), &mut sink);

        assert_eq!(
            room.last_check_at(),
            Some(1_700_000_500.0),
            "re-sending a known check is not activity"
        );
    }

    /// The room-wide value is the newest across slots, not the newest of
    /// whichever slot happened to check last in insertion order.
    #[test]
    fn it_is_the_newest_across_every_slot() {
        if skip_without(FIXTURE) {
            return;
        }
        let data = load(FIXTURE).unwrap();
        let players: Vec<(u32, String, String)> = data
            .player_slots()
            .take(2)
            .map(|(s, i)| (*s, i.name.clone(), i.game.clone()))
            .collect();
        if players.len() < 2 {
            eprintln!("SKIP: fixture has fewer than two player slots");
            return;
        }

        let mut room = room_for(Arc::clone(&data), RoomOptions::default());
        let mut sink = Recorder::default();
        let conns: Vec<ConnId> = players
            .iter()
            .enumerate()
            .map(|(i, (_, name, game))| join(&mut room, i as u64 + 1, name, game, 0b111))
            .collect();

        // The *second* slot checks first, so a naive "last one written" would
        // still pass; only a real max over the values gets this right.
        let later = room.multidata().locations.for_slot(players[1].0)[0].location;
        room.tick(1_700_009_000.0, &mut sink);
        room.handle(conns[1], checks(vec![later]), &mut sink);

        let earlier = room.multidata().locations.for_slot(players[0].0)[0].location;
        room.tick(1_700_001_000.0, &mut sink);
        room.handle(conns[0], checks(vec![earlier]), &mut sink);

        assert_eq!(
            room.last_check_at(),
            Some(1_700_009_000.0),
            "the room-wide timer is the newest check anywhere, not the last one recorded"
        );
    }

    /// It survives a restart, which is the reason puna reads it from the room
    /// rather than reconstructing it from polled check counts.
    #[test]
    fn it_survives_a_save_and_restore() {
        if skip_without(FIXTURE) {
            return;
        }
        let (mut room, conn, slot) = setup().unwrap();
        let first = room.multidata().locations.for_slot(slot)[0].location;

        let mut sink = Recorder::default();
        room.tick(1_700_000_500.0, &mut sink);
        room.handle(conn, checks(vec![first]), &mut sink);

        let snapshot = room.snapshot();
        let data = load(FIXTURE).unwrap();
        let mut restored = room_for(data, RoomOptions::default());
        restored
            .restore(snapshot)
            .expect("a snapshot this room just produced");

        assert_eq!(
            restored.last_check_at(),
            Some(1_700_000_500.0),
            "an orchestrator's idle clock must not reset when a room restarts"
        );
    }
}
