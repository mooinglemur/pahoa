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
    serde_json::to_string(packets).expect("server packets are always serialisable")
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
    use crate::server::{LocationInfo, RoomUpdate};
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
                assert_eq!(c.name, "Alice");
                assert_eq!(c.version, Version::new(0, 6, 8));
                assert_eq!(c.items_handling, 7);
                assert!(c.slot_data, "slot_data defaults to true");
                assert_eq!(c.password, None);
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
                assert_eq!(g.keys, ["a"]);
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
                assert_eq!(s.operations[0].operation, "add");
            }
            other => panic!("got {other:?}"),
        }
    }
}
