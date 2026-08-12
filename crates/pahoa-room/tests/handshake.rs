//! Connect, refusal, and the session lifecycle.

mod common;

use common::*;
use pahoa_proto::server::ConnectionRefusedReason as Refused;
use pahoa_proto::types::Version;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

fn room(options: RoomOptions) -> Option<(Room, u32, String, String)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    Some((room_for(data, options), slot, name, game))
}

fn refusal(sink: &Recorder, conn: ConnId) -> Vec<Refused> {
    sink.packets_for(conn)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::ConnectionRefused(r) => Some(r.errors.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn room_info_arrives_before_any_authentication() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = room(RoomOptions::default()).unwrap();
    let mut sink = Recorder::default();

    room.on_connect(ConnId(1), &mut sink);

    let packets = sink.packets_for(ConnId(1));
    assert_eq!(packets.len(), 1);
    match packets[0] {
        ServerPacket::RoomInfo(info) => {
            assert!(!info.games.is_empty());
            // Always advertised, because the server itself grants items from it.
            assert!(info.games.contains(&"Archipelago".to_string()));
            assert!(!info.password, "no password configured");
        }
        other => panic!("expected RoomInfo, got {}", packet_name(other)),
    }
}

#[test]
fn room_info_reports_whether_a_password_is_set() {
    if skip_without(FIXTURE) {
        return;
    }
    let opts = RoomOptions {
        password: Some("hunter2".into()),
        ..Default::default()
    };
    let (mut room, ..) = room(opts).unwrap();
    let mut sink = Recorder::default();
    room.on_connect(ConnId(1), &mut sink);

    match sink.packets_for(ConnId(1))[0] {
        ServerPacket::RoomInfo(info) => assert!(info.password),
        other => panic!("expected RoomInfo, got {}", packet_name(other)),
    }
}

#[test]
fn a_valid_connect_is_accepted() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, name, game) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(conn, connect(&name, &game, 0b001), &mut sink);

    let connected = sink
        .packets_for(conn)
        .into_iter()
        .find_map(|p| match p {
            ServerPacket::Connected(c) => Some(c),
            _ => None,
        })
        .expect("should have been accepted");

    assert_eq!(connected.slot, slot);
    assert_eq!(connected.team, 0);
    assert!(
        !connected.missing_locations.is_empty(),
        "nothing checked yet"
    );
    assert!(connected.checked_locations.is_empty());
    assert!(room.client(conn).unwrap().auth);
}

#[test]
fn an_unknown_slot_name_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, _, game) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(conn, connect("Nobody At All", &game, 0b001), &mut sink);
    assert_eq!(refusal(&sink, conn), [Refused::InvalidSlot]);
    assert!(!room.client(conn).unwrap().auth);
}

#[test]
fn the_wrong_game_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, _) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(conn, connect(&name, "Not A Real Game", 0b001), &mut sink);
    assert_eq!(refusal(&sink, conn), [Refused::InvalidGame]);
}

#[test]
fn a_wrong_password_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let opts = RoomOptions {
        password: Some("hunter2".into()),
        ..Default::default()
    };
    let (mut room, _, name, game) = room(opts).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(conn, connect(&name, &game, 0b001), &mut sink);
    assert_eq!(refusal(&sink, conn), [Refused::InvalidPassword]);
}

#[test]
fn an_old_client_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, game) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    let ClientPacket::Connect(mut c) = connect(&name, &game, 0b001) else {
        unreachable!()
    };
    c.version = Version::new(0, 1, 0);
    room.handle(conn, ClientPacket::Connect(c), &mut sink);

    assert_eq!(refusal(&sink, conn), [Refused::IncompatibleVersion]);
}

#[test]
fn invalid_items_handling_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, game) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    // 0b110 sets "own world" and "start inventory" without the base bit that
    // both depend on.
    room.handle(conn, connect(&name, &game, 0b110), &mut sink);
    assert_eq!(refusal(&sink, conn), [Refused::InvalidItemsHandling]);
}

#[test]
fn several_problems_are_reported_together() {
    if skip_without(FIXTURE) {
        return;
    }
    let opts = RoomOptions {
        password: Some("hunter2".into()),
        ..Default::default()
    };
    let (mut room, ..) = room(opts).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(conn, connect("Nobody", "Nothing", 0b001), &mut sink);

    let errors = refusal(&sink, conn);
    assert!(errors.contains(&Refused::InvalidPassword), "{errors:?}");
    assert!(errors.contains(&Refused::InvalidSlot), "{errors:?}");
    // Game and version are only checked once the slot resolves.
    assert_eq!(errors.len(), 2, "{errors:?}");
}

#[test]
fn a_tracker_may_connect_without_naming_a_game() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, _) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(
        conn,
        ClientPacket::Connect(Box::new(cmd::Connect {
            password: None,
            game: None,
            name: name.clone(),
            uuid: serde_json::json!(null),
            // Below the per-slot floor, but a game-less tracker is held only to
            // the global minimum.
            version: Version::new(0, 5, 0),
            items_handling: 0,
            tags: vec!["Tracker".to_string()],
            slot_data: false,
        })),
        &mut sink,
    );

    assert!(
        refusal(&sink, conn).is_empty(),
        "tracker should be accepted"
    );
    assert!(
        room.client(conn).unwrap().no_locations,
        "trackers cannot check locations"
    );
}

#[test]
fn compatibility_zero_demands_an_exact_version_match() {
    if skip_without(FIXTURE) {
        return;
    }
    let opts = RoomOptions {
        compatibility: 0,
        ..Default::default()
    };
    let (mut room, _, name, game) = room(opts).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    // Newer than the floor, but not identical to the server.
    let ClientPacket::Connect(mut c) = connect(&name, &game, 0b001) else {
        unreachable!()
    };
    c.version = Version::new(0, 6, 7);
    room.handle(conn, ClientPacket::Connect(c), &mut sink);

    assert_eq!(refusal(&sink, conn), [Refused::IncompatibleVersion]);
}

#[test]
fn slot_data_is_omitted_when_the_client_declines_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, game) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    let ClientPacket::Connect(mut c) = connect(&name, &game, 0b001) else {
        unreachable!()
    };
    c.slot_data = false;
    room.handle(conn, ClientPacket::Connect(c), &mut sink);

    let connected = sink
        .packets_for(conn)
        .into_iter()
        .find_map(|p| match p {
            ServerPacket::Connected(c) => Some(c),
            _ => None,
        })
        .unwrap();
    assert!(connected.slot_data.is_none());
}

#[test]
fn joining_is_announced_once_but_reconnecting_is_not() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, game) = room(RoomOptions::default()).unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    sink.clear();

    room.handle(conn, connect(&name, &game, 0b001), &mut sink);
    let joins = sink.broadcasts().count();
    assert_eq!(joins, 1, "first Connect announces the join");

    sink.clear();
    // Re-authenticating to the same slot must not print a second join.
    room.handle(conn, connect(&name, &game, 0b001), &mut sink);
    assert_eq!(sink.broadcasts().count(), 0, "re-Connect should be silent");
}

#[test]
fn two_clients_may_share_one_slot() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, game) = room(RoomOptions::default()).unwrap();
    let a = join(&mut room, 1, &name, &game, 0b001);
    let b = join(&mut room, 2, &name, &game, 0b001);

    assert!(room.client(a).unwrap().auth);
    assert!(room.client(b).unwrap().auth);
    assert_eq!(room.all_conns(), vec![a, b]);
}

#[test]
fn disconnecting_removes_the_connection() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, _, name, game) = room(RoomOptions::default()).unwrap();
    let conn = join(&mut room, 1, &name, &game, 0b001);
    let mut sink = Recorder::default();

    room.on_disconnect(conn, &mut sink);

    assert!(room.client(conn).is_none());
    assert!(room.all_conns().is_empty());
}
