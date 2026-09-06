//! Which malformed arguments are answered, and which close the socket.
//!
//! `process_client_cmd` does one of two things with a wrongly-typed argument,
//! and which one is a property of the individual *field*:
//!
//! - a guarded `if` answers `InvalidPacket` and reads the next frame;
//! - anything indexed unguarded raises out of the read loop and the socket dies
//!   (`MultiServer.py:900-917`).
//!
//! pahoa could only ever do the second, because `serde` failed the packet
//! before the room saw it — so `"want_reply": 0`, `"password": 0` and a null
//! `name` each cost a player their connection where Archipelago would have told
//! them what was wrong. Those were live reports, not hypotheticals.
//!
//! These drive whole frames through `decode` rather than building packets, so
//! what is under test is the seam that was broken: the decoder must hand the
//! bad value *to the room*, and the room must pick the reference's answer.

mod common;

use common::*;
use pahoa_proto::server::ConnectionRefusedReason as Refused;
use pahoa_proto::{ClientPacket, ServerPacket, decode};
use pahoa_room::{CloseReason, ConnId, Event, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

/// What one frame produced: the `InvalidPacket` texts, and whether it closed.
#[derive(Debug, Default, PartialEq)]
struct Outcome {
    refused: Vec<String>,
    closed: bool,
}

impl Outcome {
    fn answered(text: &str) -> Self {
        Self {
            refused: vec![text.to_string()],
            closed: false,
        }
    }

    fn closed() -> Self {
        Self {
            refused: vec![],
            closed: true,
        }
    }
}

/// Decode `frame` as a client would send it, hand it to the room, and report.
///
/// A decode failure counts as `closed`, because that is exactly what the actor
/// does with one (`actor.rs`'s `DecodeFailed` arm).
fn feed(room: &mut Room, conn: ConnId, frame: &str) -> Outcome {
    let mut sink = Recorder::default();
    let Ok(packets) = decode(frame) else {
        return Outcome::closed();
    };
    for packet in packets {
        room.handle(conn, packet, &mut sink);
    }
    Outcome {
        refused: sink
            .packets_for(conn, room)
            .into_iter()
            .filter_map(|p| match p {
                ServerPacket::InvalidPacket(i) => Some(i.text.clone()),
                _ => None,
            })
            .collect(),
        closed: sink
            .events
            .iter()
            .any(|e| matches!(e, Event::Close { conn: c, .. } if *c == conn)),
    }
}

/// A room with one authenticated client, since most of these commands need one.
fn setup() -> Option<(Room, ConnId)> {
    let data = load(FIXTURE)?;
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);
    Some((room, conn))
}

// --- answered, connection kept -------------------------------------------

#[test]
fn a_wrongly_typed_argument_is_answered_where_the_reference_guards_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();

    // (frame, the reference's own `text`)
    let cases = [
        // `"text" not in args or type(args["text"]) is not str` (`:2176`).
        (r#"[{"cmd":"Say","text":7}]"#, "Say"),
        (r#"[{"cmd":"Say"}]"#, "Say"),
        // `"keys" not in args or type(args["keys"]) != list` (`:2246`) — and
        // the reference answers `Retrieve` here, not the command's own name.
        (r#"[{"cmd":"Get","keys":"a"}]"#, "Retrieve"),
        (r#"[{"cmd":"Get"}]"#, "Retrieve"),
        (r#"[{"cmd":"SetNotify","keys":7}]"#, "SetNotify"),
        (r#"[{"cmd":"SetNotify"}]"#, "SetNotify"),
        // `"key" not in args or … not type(args["operations"]) == list` (`:2261`).
        (r#"[{"cmd":"Set","operations":[]}]"#, "Set"),
        (r#"[{"cmd":"Set","key":"k","operations":7}]"#, "Set"),
        (r#"[{"cmd":"Set","key":"k"}]"#, "Set"),
        // Each `Bounce` filter carries its own message (`:2185-2211`).
        (
            r#"[{"cmd":"Bounce","games":"Timespinner","data":{}}]"#,
            "Bounce: Games list provided did not have the correct format.",
        ),
        (
            r#"[{"cmd":"Bounce","tags":[7],"data":{}}]"#,
            "Bounce: Tags list provided did not have the correct format.",
        ),
        (
            r#"[{"cmd":"Bounce","slots":["x"],"data":{}}]"#,
            "Bounce: Slots list provided did not have the correct format.",
        ),
        // `isinstance` on all three at once (`:2126`).
        (
            r#"[{"cmd":"UpdateHint","player":"x","location":1,"status":null}]"#,
            "UpdateHint",
        ),
        (
            r#"[{"cmd":"UpdateHint","player":1,"location":{},"status":null}]"#,
            "UpdateHint",
        ),
        (
            r#"[{"cmd":"UpdateHint","player":1,"location":1,"status":"x"}]"#,
            "UpdateHint",
        ),
        // Each element is checked, with its own message (`:2052-2058`). The
        // type test comes *before* the table lookup, so a junk element is
        // reported even though a real-but-unknown id would raise.
        (
            r#"[{"cmd":"LocationScouts","locations":["x"]}]"#,
            "Locations has to be a list of integers",
        ),
        (
            r#"[{"cmd":"LocationScouts","locations":[null]}]"#,
            "Locations has to be a list of integers",
        ),
    ];

    for (frame, want) in cases {
        assert_eq!(
            feed(&mut room, conn, frame),
            Outcome::answered(want),
            "{frame}"
        );
    }
}

/// The `Set` that started this: `"want_reply": 0` is falsy in Python and
/// perfectly ordinary, and it was closing connections on live data-storage
/// traffic.
#[test]
fn an_integer_want_reply_is_an_ordinary_set() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();

    let frame = r#"[{"cmd":"Set","key":"k","want_reply":0,
                     "operations":[{"operation":"replace","value":1}]}]"#;
    assert_eq!(feed(&mut room, conn, frame), Outcome::default(), "{frame}");
    assert_eq!(
        room.stored_data().get("k").map(|v| (**v).clone()),
        Some(serde_json::json!(1)),
        "the operation must actually have run"
    );
}

/// `Connect` is the one whose answer is a `ConnectionRefused` rather than an
/// `InvalidPacket`, because the reference reaches its refusal list rather than
/// its guard.
#[test]
fn a_connect_with_a_null_name_is_refused_rather_than_dropped() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);

    // `args['name'] not in ctx.connect_names` is a lookup, not a type check —
    // a null simply matches no slot (`MultiServer.py:1913`).
    let frame = r#"[{"cmd":"Connect","password":null,"game":"Timespinner","name":null,
                     "uuid":"u","version":{"major":0,"minor":6,"build":8,"class":"Version"},
                     "items_handling":0,"tags":[]}]"#;
    let mut sink = Recorder::default();
    for packet in decode(frame).expect("this must not fail to decode") {
        room.handle(conn, packet, &mut sink);
    }

    let refusals: Vec<&Refused> = sink
        .packets_for(conn, &room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::ConnectionRefused(r) => Some(&r.errors),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(refusals, [&Refused::InvalidSlot], "{frame}");
    assert!(
        !sink.events.iter().any(|e| matches!(e, Event::Close { .. })),
        "the reference keeps the socket open here"
    );
}

#[test]
fn a_connect_the_reference_guards_answers_invalid_packet() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());
    let conn = ConnId(1);
    room.on_connect(conn, &mut Recorder::default());

    // `'password' not in args or type(args['password']) not in [str, NoneType]
    //  or 'game' not in args` (`MultiServer.py:1904-1907`).
    let head = r#""name":"x","uuid":"u",
                  "version":{"major":0,"minor":6,"build":8,"class":"Version"},
                  "items_handling":0,"tags":[]"#;
    for bad in [
        format!(r#"[{{"cmd":"Connect","game":"G",{head}}}]"#),
        format!(r#"[{{"cmd":"Connect","password":7,"game":"G",{head}}}]"#),
        format!(r#"[{{"cmd":"Connect","password":null,{head}}}]"#),
    ] {
        assert_eq!(
            feed(&mut room, conn, &bad),
            Outcome::answered("Connect"),
            "{bad}"
        );
    }
}

/// A wrongly-typed `items_handling` reaches the reference's setter, whose
/// `value & 0b001` raises `TypeError` into the `except` that adds
/// `InvalidItemsHandling` — so it is a refusal, not a disconnect.
#[test]
fn a_wrongly_typed_items_handling_is_refused_like_a_bad_flag_combination() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = ConnId(1);
    room.on_connect(conn, &mut Recorder::default());

    let frame = format!(
        r#"[{{"cmd":"Connect","password":null,"game":"{game}","name":"{name}","uuid":"u",
              "version":{{"major":0,"minor":6,"build":8,"class":"Version"}},
              "items_handling":"seven","tags":[]}}]"#
    );
    let mut sink = Recorder::default();
    for packet in decode(&frame).expect("decodes") {
        room.handle(conn, packet, &mut sink);
    }

    let refusals: Vec<Refused> = sink
        .packets_for(conn, &room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::ConnectionRefused(r) => Some(r.errors.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(refusals, [Refused::InvalidItemsHandling], "{frame}");
}

// --- ignored -------------------------------------------------------------

/// `register_location_checks` intersects the list with the slot's locations
/// (`MultiServer.py:2042-2045`) — nothing type-checks it, so junk matches
/// nothing and the *rest of the batch* still registers.
///
/// That last part is the point. Refusing the frame would lose a player every
/// check in it over one bad element.
#[test]
fn junk_in_a_location_check_is_dropped_and_the_rest_still_counts() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let real: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(2)
        .map(|e| e.location)
        .collect();
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let frame = format!(
        r#"[{{"cmd":"LocationChecks","locations":[{},null,"x",1.5,{}]}}]"#,
        real[0], real[1]
    );
    assert_eq!(feed(&mut room, conn, &frame), Outcome::default(), "{frame}");
    assert_eq!(
        room.checked_count((0, slot)),
        2,
        "both real locations must have registered"
    );
}

// --- still fatal ---------------------------------------------------------

/// The other half of the contract. Being lenient about the guarded fields must
/// not quietly turn the *unguarded* ones lenient too — those raise in the
/// reference, and a server that answers where Archipelago closes is as wrong as
/// one that closes where Archipelago answers.
#[test]
fn what_the_reference_raises_on_still_closes_the_socket() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();

    let cases = [
        // `args["key"].startswith(...)` on a non-string (`:2261`).
        r#"[{"cmd":"Set","key":7,"operations":[]}]"#,
        // `operation["operation"]` on something that is not a mapping (`:2269`).
        r#"[{"cmd":"Set","key":"k","operations":[7]}]"#,
        r#"[{"cmd":"Set","key":"k","operations":[{"value":1}]}]"#,
        // `key.startswith("_read_")` on a non-string key (`:2253`).
        r#"[{"cmd":"Get","keys":[7]}]"#,
        // Indexed unguarded, so an absent one is a `KeyError` (`:2126`).
        r#"[{"cmd":"UpdateHint","player":1,"location":1}]"#,
        r#"[{"cmd":"UpdateHint","player":1,"status":null}]"#,
        // `for location in args["locations"]` (`:2047`).
        r#"[{"cmd":"LocationScouts"}]"#,
        r#"[{"cmd":"LocationChecks"}]"#,
        r#"[{"cmd":"LocationChecks","locations":7}]"#,
    ];

    for frame in cases {
        assert_eq!(feed(&mut room, conn, frame), Outcome::closed(), "{frame}");
    }
}

/// `Connect` fields the reference indexes rather than guards.
#[test]
fn a_connect_missing_an_unguarded_field_still_closes_the_socket() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());
    let conn = ConnId(1);
    room.on_connect(conn, &mut Recorder::default());

    let fields = [
        ("password", "null"),
        ("game", r#""G""#),
        ("name", r#""x""#),
        ("uuid", r#""u""#),
        (
            "version",
            r#"{"major":0,"minor":6,"build":8,"class":"Version"}"#,
        ),
        ("items_handling", "0"),
        ("tags", "[]"),
    ];
    // The whole thing decodes; dropping one unguarded key at a time must not.
    let render = |omit: &str| {
        let body: Vec<String> = fields
            .iter()
            .filter(|(k, _)| *k != omit)
            .map(|(k, v)| format!(r#""{k}":{v}"#))
            .collect();
        format!(r#"[{{"cmd":"Connect",{}}}]"#, body.join(","))
    };
    assert!(decode(&render("")).is_ok(), "the control must decode");
    for key in ["name", "uuid", "version", "items_handling"] {
        let frame = render(key);
        assert!(
            decode(&frame).is_err(),
            "an absent {key} raises in the reference: {frame}"
        );
    }
    let _ = conn;
}

/// A closed socket must say *why* in the log, since nothing reaches the client.
#[test]
fn a_fatal_argument_names_itself_in_the_close_reason() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    let mut sink = Recorder::default();
    for packet in decode(r#"[{"cmd":"Set","key":7,"operations":[]}]"#).unwrap() {
        room.handle(conn, packet, &mut sink);
    }
    let reason = sink
        .events
        .iter()
        .find_map(|e| match e {
            Event::Close {
                reason: CloseReason::ProtocolError(text),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .expect("closed with a protocol error");
    assert!(reason.contains("key"), "{reason}");
}

/// `Sync` and the rest are untouched: this is about malformed arguments, not
/// about loosening what a well-formed packet does.
#[test]
fn well_formed_packets_are_unaffected() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn) = setup().unwrap();
    for frame in [
        r#"[{"cmd":"Sync"}]"#,
        r#"[{"cmd":"Say","text":"hello"}]"#,
        r#"[{"cmd":"Get","keys":["k"]}]"#,
        r#"[{"cmd":"SetNotify","keys":["k"]}]"#,
        r#"[{"cmd":"Bounce","tags":["DeathLink"],"data":{}}]"#,
        r#"[{"cmd":"Set","key":"k","operations":[{"operation":"replace","value":1}]}]"#,
    ] {
        assert_eq!(feed(&mut room, conn, frame), Outcome::default(), "{frame}");
    }
    // And the decoded form is still what the room acts on.
    assert!(matches!(
        decode(r#"[{"cmd":"Sync"}]"#).unwrap()[0],
        ClientPacket::Sync
    ));
}
