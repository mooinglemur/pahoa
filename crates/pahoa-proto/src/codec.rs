//! Frame encoding and decoding.
//!
//! A frame is a JSON **array** of command objects, not a single object: a
//! client may batch several commands into one WebSocket message, and the server
//! processes them in order (`MultiServer.py:910-911`). Output is compact —
//! serde_json's default separators already match Python's
//! `separators=(',',':')`, and its UTF-8 passthrough matches `ensure_ascii=False`.

use crate::client::*;
use crate::depth;
use crate::server::ServerPacket;
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error(transparent)]
    Depth(#[from] depth::DepthError),

    #[error("frame is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("frame must be a JSON array of commands, found {found}")]
    NotAnArray { found: &'static str },

    #[error("command {index} is not an object, found {found}")]
    NotAnObject { index: usize, found: &'static str },

    #[error("command {index} has no cmd field")]
    MissingCmd { index: usize },

    #[error("command {index}: cmd must be a string")]
    CmdNotAString { index: usize },

    /// Reported rather than dropped so the caller can answer with
    /// `InvalidPacket{type:"cmd"}`, which is what Archipelago does.
    #[error("unknown command {cmd:?}")]
    UnknownCmd { cmd: String },

    #[error("{cmd}: {source}")]
    BadArguments {
        cmd: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Encode a batch of packets as one frame.
pub fn encode(packets: &[ServerPacket]) -> String {
    serde_json::to_string(packets).expect("server packets are always serializable")
}

/// Decode one inbound frame.
///
/// The depth guard runs first, on the raw text, so a hostile deeply-nested
/// payload never reaches the JSON parser.
pub fn decode(frame: &str) -> Result<Vec<ClientPacket>, DecodeError> {
    depth::check(frame)?;

    let value: Value = serde_json::from_str(frame)?;
    let Value::Array(items) = value else {
        return Err(DecodeError::NotAnArray {
            found: type_name(&value),
        });
    };

    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let Value::Object(map) = item else {
            return Err(DecodeError::NotAnObject {
                index,
                found: type_name(&item),
            });
        };
        out.push(decode_one(index, map)?);
    }
    Ok(out)
}

fn decode_one(index: usize, map: Map<String, Value>) -> Result<ClientPacket, DecodeError> {
    let cmd = map.get("cmd").ok_or(DecodeError::MissingCmd { index })?;
    let cmd = cmd
        .as_str()
        .ok_or(DecodeError::CmdNotAString { index })?
        .to_string();

    // Deserialising from the map by value would move it; the echo commands need
    // it kept, so clone only where that matters.
    macro_rules! parse {
        ($name:literal, $ty:ty) => {
            serde_json::from_value::<$ty>(Value::Object(map.clone()))
                .map_err(|source| DecodeError::BadArguments { cmd: $name, source })?
        };
    }

    Ok(match cmd.as_str() {
        "Connect" => ClientPacket::Connect(Box::new(parse!("Connect", Connect))),
        "ConnectUpdate" => ClientPacket::ConnectUpdate(parse!("ConnectUpdate", ConnectUpdate)),
        "Sync" => ClientPacket::Sync,
        "LocationChecks" => ClientPacket::LocationChecks(parse!("LocationChecks", LocationChecks)),
        "LocationScouts" => ClientPacket::LocationScouts(parse!("LocationScouts", LocationScouts)),
        "CreateHints" => ClientPacket::CreateHints(parse!("CreateHints", CreateHints)),
        "UpdateHint" => ClientPacket::UpdateHint(parse!("UpdateHint", UpdateHint)),
        "StatusUpdate" => ClientPacket::StatusUpdate(parse!("StatusUpdate", StatusUpdate)),
        "Say" => ClientPacket::Say(parse!("Say", Say)),
        "GetDataPackage" => ClientPacket::GetDataPackage(parse!("GetDataPackage", GetDataPackage)),
        "Bounce" => ClientPacket::Bounce(parse!("Bounce", Bounce), map),
        "Get" => ClientPacket::Get(parse!("Get", Get), map),
        "Set" => ClientPacket::Set(Box::new(parse!("Set", Set)), map),
        "SetNotify" => ClientPacket::SetNotify(parse!("SetNotify", SetNotify)),
        _ => return Err(DecodeError::UnknownCmd { cmd }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arg;
    use crate::server::{LocationInfo, RoomUpdate};

    /// The raw `operations` list of a decoded `Set`.
    fn ops(s: &Set) -> &[Value] {
        s.operations.as_ok().expect("operations decoded")
    }
    use crate::types::Version;

    #[test]
    fn encodes_a_batch_as_one_array() {
        let packets = vec![
            ServerPacket::LocationInfo(LocationInfo { locations: vec![] }),
            ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                hint_points: Some(3),
                ..Default::default()
            })),
        ];
        assert_eq!(
            encode(&packets),
            r#"[{"cmd":"LocationInfo","locations":[]},{"cmd":"RoomUpdate","hint_points":3}]"#
        );
    }

    #[test]
    fn output_is_compact_and_does_not_escape_non_ascii() {
        // Matches Python's separators=(',',':') and ensure_ascii=False.
        let p = ServerPacket::PrintJSON(crate::server::PrintJson {
            data: vec![crate::types::JsonMessagePart::text("héllo ✓")],
            ..Default::default()
        });
        let s = encode(std::slice::from_ref(&p));
        assert!(s.contains("héllo ✓"), "{s}");
        assert!(!s.contains(", "), "{s}");
        assert!(!s.contains(": "), "{s}");
    }

    #[test]
    fn decodes_a_batch_of_commands() {
        let frame = r#"[{"cmd":"Sync"},{"cmd":"LocationChecks","locations":[1,2,3]}]"#;
        let packets = decode(frame).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], ClientPacket::Sync);
        match &packets[1] {
            ClientPacket::LocationChecks(l) => assert_eq!(l.locations, [1, 2, 3]),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn decodes_connect_with_a_tagged_version() {
        let frame = r#"[{"cmd":"Connect","password":null,"game":"Timespinner","name":"Alice",
                        "uuid":"abc","version":{"major":0,"minor":6,"build":8,"class":"Version"},
                        "items_handling":7,"tags":["AP"]}]"#;
        let packets = decode(frame).unwrap();
        match &packets[0] {
            ClientPacket::Connect(c) => {
                assert_eq!(c.name, Arg::Ok("Alice".into()));
                assert_eq!(c.version, Version::new(0, 6, 8));
                assert_eq!(c.items_handling, Arg::Ok(crate::lenient::U8(7)));
                assert!(c.slot_data, "slot_data defaults to true");
                assert_eq!(c.password, Arg::Ok(None));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn echo_commands_retain_their_original_map() {
        // The reply is this map mutated in place, so unknown keys must survive.
        let frame = r#"[{"cmd":"Get","keys":["a"],"client_tag":7}]"#;
        match &decode(frame).unwrap()[0] {
            ClientPacket::Get(g, raw) => {
                assert_eq!(g.keys, Arg::Ok(vec![serde_json::json!("a")]));
                assert_eq!(raw.get("client_tag"), Some(&serde_json::json!(7)));
                // Order is preserved for byte-identical echoes.
                assert_eq!(
                    raw.keys().collect::<Vec<_>>(),
                    ["cmd", "keys", "client_tag"]
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_frames_that_are_not_arrays() {
        // A lone object is the most likely client mistake.
        assert!(matches!(
            decode(r#"{"cmd":"Sync"}"#),
            Err(DecodeError::NotAnArray { found: "object" })
        ));
    }

    #[test]
    fn reports_unknown_commands_by_name() {
        match decode(r#"[{"cmd":"Nonsense"}]"#) {
            Err(DecodeError::UnknownCmd { cmd }) => assert_eq!(cmd, "Nonsense"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn enforces_the_depth_limit_before_parsing() {
        let frame = "[".repeat(20) + &"]".repeat(20);
        assert!(matches!(decode(&frame), Err(DecodeError::Depth(_))));
    }

    #[test]
    fn reports_which_command_had_bad_arguments() {
        match decode(r#"[{"cmd":"LocationChecks","locations":"nope"}]"#) {
            Err(DecodeError::BadArguments {
                cmd: "LocationChecks",
                ..
            }) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn only_connect_and_getdatapackage_are_allowed_before_auth() {
        let frame = r#"[{"cmd":"GetDataPackage"},{"cmd":"Sync"}]"#;
        let packets = decode(frame).unwrap();
        assert!(packets[0].allowed_before_auth());
        assert!(!packets[1].allowed_before_auth());
    }

    #[test]
    fn set_defaults_want_reply_to_false() {
        let frame = r#"[{"cmd":"Set","key":"k","operations":[{"operation":"add","value":1}]}]"#;
        match &decode(frame).unwrap()[0] {
            ClientPacket::Set(s, _) => {
                assert!(!s.want_reply);
                assert_eq!(ops(s)[0]["operation"], "add");
            }
            other => panic!("got {other:?}"),
        }
    }

    // --- integers a client spells as floats ------------------------------
    //
    // Seen live: a client sending `"locations": [113.0]`. Python's json makes
    // that a float, `113.0 == 113` with the same hash, and the reference server
    // indexes its location table with it none the wiser. Rust asked, refused,
    // and dropped the connection on every location the player checked.

    #[test]
    fn location_checks_accepts_integral_floats() {
        let packets = decode(r#"[{"cmd":"LocationChecks","locations":[113.0,221.0,42]}]"#)
            .expect("the reference accepts this, so we must");
        match &packets[0] {
            ClientPacket::LocationChecks(c) => {
                // Held raw, so the room can drop what does not resolve rather
                // than lose the whole batch; `lenient::as_int` reads them.
                let ids: Vec<i64> = c
                    .locations
                    .iter()
                    .filter_map(crate::lenient::as_int)
                    .collect();
                assert_eq!(ids, vec![113, 221, 42]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// **Truncating would be worse than dropping.** A fractional location id is
    /// not a location any seed has, and rounding one into a real id would check
    /// somebody's location because a client had a rounding bug. The reference
    /// reaches the same place from the other side: `113.5` simply matches
    /// nothing in its table.
    #[test]
    fn a_fractional_location_resolves_to_nothing() {
        for junk in ["113.5", "1e300", r#""113""#, "null"] {
            let frame = format!(r#"[{{"cmd":"LocationChecks","locations":[{junk},42]}}]"#);
            let packets = decode(&frame).unwrap_or_else(|e| panic!("{junk} -> {e}"));
            match &packets[0] {
                ClientPacket::LocationChecks(c) => {
                    let ids: Vec<i64> = c
                        .locations
                        .iter()
                        .filter_map(crate::lenient::as_int)
                        .collect();
                    assert_eq!(
                        ids,
                        vec![42],
                        "{junk} must resolve to nothing, and 42 survive"
                    );
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// `LocationScouts` is the one that reports the junk instead of ignoring
    /// it, because the reference checks that list element by element with its
    /// own message — so the element has to survive decoding either way.
    #[test]
    fn a_scouted_list_keeps_its_junk_for_the_handler_to_report() {
        let packets = decode(r#"[{"cmd":"LocationScouts","locations":[113.5]}]"#)
            .expect("the handler answers this, so decoding must not refuse it");
        match &packets[0] {
            ClientPacket::LocationScouts(s) => {
                assert_eq!(s.locations.len(), 1);
                assert_eq!(crate::lenient::as_int(&s.locations[0]), None);
            }
            other => panic!("{other:?}"),
        }
    }

    // --- integers a client spells as booleans -----------------------------
    //
    // Seen live: `"create_as_hint": false`, dropping a socket every time. The
    // reference reads it through `int(args.get("create_as_hint", 0))`, and
    // `int(False)` is 0 — the ordinary "scout, do not hint" request — because
    // `bool` is a subclass of `int` in Python.

    #[test]
    fn create_as_hint_accepts_the_boolean_spelling() {
        for (spelling, expected) in [("false", 0), ("true", 1), ("2", 2)] {
            let frame = format!(
                r#"[{{"cmd":"LocationScouts","locations":[7],"create_as_hint":{spelling}}}]"#
            );
            let packets = decode(&frame).unwrap_or_else(|e| panic!("{spelling} -> {e}"));
            match &packets[0] {
                ClientPacket::LocationScouts(s) => {
                    assert_eq!(s.create_as_hint, expected, "create_as_hint: {spelling}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn every_inbound_number_tolerates_the_boolean_spelling() {
        // Each of these reaches Python arithmetic, an `isinstance(_, int)`, or
        // a dict lookup — all of which take a bool without comment.
        let cases = [
            r#"[{"cmd":"LocationScouts","locations":[7],"create_as_hint":true}]"#,
            r#"[{"cmd":"CreateHints","locations":[7],"player":true,"status":false}]"#,
            r#"[{"cmd":"UpdateHint","player":true,"location":true,"status":false}]"#,
            r#"[{"cmd":"StatusUpdate","status":false}]"#,
            r#"[{"cmd":"Connect","password":null,"game":"G","name":"n","uuid":"u",
                 "version":{"major":0,"minor":6,"build":8,"class":"Version"},
                 "items_handling":true,"tags":[]}]"#,
        ];
        for case in cases {
            decode(case).unwrap_or_else(|e| panic!("{case} -> {e}"));
        }
    }

    /// **The one place a boolean is *not* an integer**, and it is upstream's
    /// distinction rather than ours: `LocationScouts` screens its ids with
    /// `type(location) is not int`, and `type(True)` is `bool`. Two lines away
    /// `isinstance` would have said yes.
    #[test]
    fn a_boolean_location_id_is_not_an_integer() {
        let packets = decode(r#"[{"cmd":"LocationScouts","locations":[true,7]}]"#)
            .expect("the handler answers this, so decoding must not refuse it");
        match &packets[0] {
            ClientPacket::LocationScouts(s) => {
                assert_eq!(crate::lenient::as_int(&s.locations[0]), None, "a bool id");
                assert_eq!(crate::lenient::as_int(&s.locations[1]), Some(7));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_inbound_number_tolerates_the_float_spelling() {
        let cases = [
            r#"[{"cmd":"LocationScouts","locations":[7.0],"create_as_hint":2.0}]"#,
            r#"[{"cmd":"CreateHints","locations":[7.0],"player":3.0,"status":20.0}]"#,
            r#"[{"cmd":"UpdateHint","player":3.0,"location":7.0,"status":20.0}]"#,
            r#"[{"cmd":"StatusUpdate","status":30.0}]"#,
        ];
        for case in cases {
            decode(case).unwrap_or_else(|e| panic!("{case} -> {e}"));
        }
    }

    // --- booleans a client spells as something else -----------------------
    //
    // Seen live: `Set: invalid type: integer 0, expected a boolean`, repeatedly,
    // on a room's ordinary data-storage traffic. The reference writes
    // `if args.get("want_reply", False):` — a truth test on whatever `json`
    // produced, so `0` is simply false there and the packet is unremarkable.

    #[test]
    fn want_reply_accepts_anything_python_would_test_for_truth() {
        let set = r#"[{"cmd":"Set","key":"k","operations":[],"want_reply":WR}]"#;
        for (spelling, expected) in [
            ("0", false),
            ("1", true),
            ("2", true),
            ("-1", true),
            ("0.0", false),
            ("0.5", true),
            (r#""""#, false),
            (r#""no""#, true), // a non-empty string is true, however it reads
            ("[]", false),
            ("[1,2]", true),
            ("{}", false),
            (r#"{"a":1}"#, true),
            ("null", false),
            ("false", false),
            ("true", true),
        ] {
            let json = set.replace("WR", spelling);
            let packets = decode(&json).unwrap_or_else(|e| panic!("{spelling} -> {e}"));
            match &packets[0] {
                ClientPacket::Set(s, _) => {
                    assert_eq!(s.want_reply, expected, "want_reply: {spelling}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// A container has to be drained, not peeked at, or the fields after it
    /// fail to parse.
    #[test]
    fn a_container_spelling_does_not_derail_the_rest_of_the_packet() {
        let packets = decode(
            r#"[{"cmd":"Set","key":"k","want_reply":[1,2,3],"operations":[{"operation":"add","value":1}]}]"#,
        )
        .expect("decodes");
        match &packets[0] {
            ClientPacket::Set(s, _) => {
                assert!(s.want_reply);
                assert_eq!(ops(s).len(), 1, "the field after it survived");
                assert_eq!(ops(s)[0]["operation"], "add");
            }
            other => panic!("{other:?}"),
        }
    }

    /// `slot_data` is the other truth test (`MultiServer.py:1973`), and its
    /// default is the interesting half: absent means *yes*, while an explicit
    /// null is falsy and means no.
    #[test]
    fn slot_data_defaults_to_true_but_an_explicit_null_is_false() {
        let connect = r#"[{"cmd":"Connect","password":null,"game":"G","name":"n","uuid":"u",
            "version":{"major":0,"minor":6,"build":8,"class":"Version"},"items_handling":0 EXTRA}]"#;
        for (extra, expected) in [
            ("", true),
            (r#","slot_data":0"#, false),
            (r#","slot_data":null"#, false),
            (r#","slot_data":1"#, true),
            (r#","slot_data":false"#, false),
        ] {
            let json = connect.replace("EXTRA", extra);
            let packets = decode(&json).unwrap_or_else(|e| panic!("{extra:?} -> {e}"));
            match &packets[0] {
                ClientPacket::Connect(c) => {
                    assert_eq!(c.slot_data, expected, "slot_data: {extra:?}")
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// The lenient reader must not quietly relax what was deliberately strict.
    ///
    /// `UpdateHint.status` is required but nullable: an explicit `null` means
    /// "leave it alone", while omitting the key raises in the reference and
    /// drops the socket. Adding a custom deserializer to an `Option` field is
    /// exactly where that distinction gets lost, since serde stops treating the
    /// field as optional unless it is also told to.
    #[test]
    fn an_absent_nullable_field_is_still_distinct_from_an_explicit_null() {
        let with_null = decode(r#"[{"cmd":"UpdateHint","player":3,"location":7,"status":null}]"#)
            .expect("an explicit null is accepted");
        match &with_null[0] {
            ClientPacket::UpdateHint(u) => assert_eq!(u.status, Arg::Ok(None)),
            other => panic!("{other:?}"),
        }
        assert!(
            decode(r#"[{"cmd":"UpdateHint","player":3,"location":7}]"#).is_err(),
            "omitting the key must still be a decode failure"
        );
    }
}
