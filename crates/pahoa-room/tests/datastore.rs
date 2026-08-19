//! Data storage and Bounce, at the room level.

mod common;

use common::*;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};
use serde_json::{Map, Value, json};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn raw(cmd_name: &str, fields: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("cmd".into(), Value::String(cmd_name.into()));
    for (k, v) in fields {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

fn get(keys: &[&str]) -> ClientPacket {
    let list: Vec<Value> = keys.iter().map(|k| json!(k)).collect();
    ClientPacket::Get(
        cmd::Get {
            keys: keys.iter().map(|k| k.to_string()).collect(),
        },
        raw("Get", &[("keys", Value::Array(list))]),
    )
}

fn set(key: &str, ops: &[(&str, Value)], want_reply: bool) -> ClientPacket {
    let operations: Vec<cmd::DataStorageOperation> = ops
        .iter()
        .map(|(o, v)| cmd::DataStorageOperation {
            operation: o.to_string(),
            value: v.clone(),
        })
        .collect();
    let ops_json: Vec<Value> = ops
        .iter()
        .map(|(o, v)| json!({"operation": o, "value": v}))
        .collect();
    ClientPacket::Set(
        Box::new(cmd::Set {
            key: key.to_string(),
            default: None,
            want_reply,
            operations,
        }),
        raw(
            "Set",
            &[
                ("key", json!(key)),
                ("want_reply", json!(want_reply)),
                ("operations", Value::Array(ops_json)),
            ],
        ),
    )
}

/// Extract the echo payloads a connection received.
fn echoes<'a>(sink: &'a Recorder, conn: ConnId, room: &Room) -> Vec<&'a Map<String, Value>> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::Echo(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn setup() -> Option<(Room, ConnId)> {
    let data = load(FIXTURE)?;
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b001);
    Some((room, conn))
}

#[test]
fn getting_an_absent_key_yields_null() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    room.handle(conn, get(&["nothing_here"]), &mut sink);

    let replies = echoes(&sink, conn, &room);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["cmd"], json!("Retrieved"));
    assert_eq!(replies[0]["keys"], json!({"nothing_here": null}));
}

#[test]
fn retrieved_carries_unknown_client_fields_through() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    // The reply is the request map mutated in place, so a client's own
    // correlation tag has to survive.
    let packet = ClientPacket::Get(
        cmd::Get {
            keys: vec!["k".into()],
        },
        raw("Get", &[("keys", json!(["k"])), ("my_tag", json!(99))]),
    );
    room.handle(conn, packet, &mut sink);

    let replies = echoes(&sink, conn, &room);
    assert_eq!(replies[0]["my_tag"], json!(99));
    // And `cmd` keeps its original position.
    assert_eq!(replies[0].keys().next().map(String::as_str), Some("cmd"));
}

#[test]
fn a_set_stores_the_value_and_reports_both_sides() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    room.handle(conn, set("counter", &[("add", json!(5))], true), &mut sink);

    let replies = echoes(&sink, conn, &room);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["cmd"], json!("SetReply"));
    // An absent key defaults to 0, not null.
    assert_eq!(replies[0]["original_value"], json!(0));
    assert_eq!(replies[0]["value"], json!(5));
    assert_eq!(*room.stored_data()["counter"], json!(5));
    assert!(sink.dirty);
}

#[test]
fn no_reply_is_sent_unless_asked_for() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    room.handle(
        conn,
        set("quiet", &[("replace", json!(1))], false),
        &mut sink,
    );

    assert!(echoes(&sink, conn, &room).is_empty());
    assert_eq!(*room.stored_data()["quiet"], json!(1));
}

#[test]
fn subscribers_are_told_even_when_the_value_is_unchanged() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let writer = join(&mut room, 1, &name, &game, 0b001);
    let watcher = join(&mut room, 2, &name, &game, 0b001);

    room.handle(
        watcher,
        ClientPacket::SetNotify(cmd::SetNotify {
            keys: vec!["shared".into()],
        }),
        &mut Recorder::default(),
    );

    let mut sink = Recorder::default();
    room.handle(
        writer,
        set("shared", &[("replace", json!(7))], false),
        &mut sink,
    );
    assert_eq!(
        echoes(&sink, watcher, &room).len(),
        1,
        "watcher should be notified"
    );

    // Setting it to the same value still notifies (`MultiServer.py:2195`).
    let mut sink = Recorder::default();
    room.handle(
        writer,
        set("shared", &[("replace", json!(7))], false),
        &mut sink,
    );
    assert_eq!(
        echoes(&sink, watcher, &room).len(),
        1,
        "unchanged writes still notify"
    );
}

#[test]
fn a_failing_operation_drops_the_connection_and_stores_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    // Modulo by zero raises in Python, which drops the socket.
    room.handle(conn, set("bad", &[("mod", json!(0))], true), &mut sink);

    let closed = sink
        .events
        .iter()
        .filter(|e| matches!(e, pahoa_room::Event::Close { .. }))
        .count();
    assert_eq!(closed, 1, "should have closed the connection");
    assert!(
        !room.stored_data().contains_key("bad"),
        "nothing should be stored"
    );
}

#[test]
fn read_only_keys_cannot_be_written() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    room.handle(
        conn,
        set("_read_race_mode", &[("replace", json!(1))], true),
        &mut sink,
    );

    let invalid = sink
        .packets_for(conn, &room)
        .into_iter()
        .filter(|p| matches!(p, ServerPacket::InvalidPacket(_)))
        .count();
    assert_eq!(invalid, 1);
}

#[test]
fn read_only_keys_expose_room_state() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let race_mode = data.race_mode;
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b001);

    let mut sink = Recorder::default();
    let status_key = format!("_read_client_status_0_{slot}");
    let slot_data_key = format!("_read_slot_data_{slot}");
    room.handle(
        conn,
        get(&["_read_race_mode", &status_key, &slot_data_key]),
        &mut sink,
    );

    let replies = echoes(&sink, conn, &room);
    let keys = &replies[0]["keys"];
    assert_eq!(keys["_read_race_mode"], json!(u8::from(race_mode)));
    // Connected, since the client just joined.
    assert_eq!(keys[&status_key], json!(5));
    assert!(
        keys.get(&slot_data_key).is_some(),
        "slot_data key should resolve"
    );
}

#[test]
fn a_bounce_reaches_everyone_carrying_the_tag_including_the_sender() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let players: Vec<(u32, String, String)> = data
        .player_slots()
        .map(|(s, i)| (*s, i.name.clone(), i.game.clone()))
        .take(3)
        .collect();
    let mut room = room_for(data, RoomOptions::default());

    // Two DeathLink clients and one without the tag.
    let a = join(&mut room, 1, &players[0].1, &players[0].2, 0b001);
    let b = join(&mut room, 2, &players[1].1, &players[1].2, 0b001);
    let c = join(&mut room, 3, &players[2].1, &players[2].2, 0b001);
    for conn in [a, b] {
        room.handle(
            conn,
            ClientPacket::ConnectUpdate(cmd::ConnectUpdate {
                items_handling: None,
                tags: Some(vec!["AP".into(), "DeathLink".into()]),
            }),
            &mut Recorder::default(),
        );
    }

    let mut sink = Recorder::default();
    let data_payload = json!({"time": 1.0, "cause": "fell", "source": "someone"});
    room.handle(
        a,
        ClientPacket::Bounce(
            cmd::Bounce {
                games: None,
                slots: None,
                tags: Some(vec!["DeathLink".into()]),
                data: data_payload.clone(),
            },
            raw(
                "Bounce",
                &[("tags", json!(["DeathLink"])), ("data", data_payload)],
            ),
        ),
        &mut sink,
    );

    // Python forwards to anyone matching, sender included.
    assert_eq!(
        echoes(&sink, a, &room).len(),
        1,
        "sender receives its own bounce"
    );
    assert_eq!(
        echoes(&sink, b, &room).len(),
        1,
        "other DeathLink client receives it"
    );
    assert_eq!(echoes(&sink, c, &room).len(), 0, "untagged client does not");

    let bounced = echoes(&sink, b, &room)[0];
    assert_eq!(bounced["cmd"], json!("Bounced"));
    assert_eq!(bounced["data"]["cause"], json!("fell"));
}

#[test]
fn a_bounce_with_no_filters_reaches_nobody() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();

    room.handle(
        conn,
        ClientPacket::Bounce(
            cmd::Bounce {
                games: None,
                slots: None,
                tags: None,
                data: json!({}),
            },
            raw("Bounce", &[("data", json!({}))]),
        ),
        &mut sink,
    );

    // Matching is `any()` over the filters, so no filters matches nothing.
    assert!(echoes(&sink, conn, &room).is_empty());
}

#[test]
fn subscriptions_are_dropped_when_a_connection_goes() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let writer = join(&mut room, 1, &name, &game, 0b001);
    let watcher = join(&mut room, 2, &name, &game, 0b001);

    room.handle(
        watcher,
        ClientPacket::SetNotify(cmd::SetNotify {
            keys: vec!["k".into()],
        }),
        &mut Recorder::default(),
    );
    room.on_disconnect(watcher, &mut Recorder::default());

    let mut sink = Recorder::default();
    room.handle(writer, set("k", &[("replace", json!(1))], false), &mut sink);
    assert!(
        echoes(&sink, watcher, &room).is_empty(),
        "a departed subscriber must not be addressed"
    );
}
