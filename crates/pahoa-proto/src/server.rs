//! Packets the server sends.
//!
//! Every variant is tagged by its `cmd` field. Three of them —
//! `Retrieved`, `SetReply` and `Bounced` — are deliberately **not** typed:
//! Archipelago builds them by mutating the client's own request map in place
//! and rebroadcasting it, unknown keys and all. See [`ServerPacket::Echo`].

use crate::types::{JsonMessagePart, NetworkItem, NetworkPlayer, NetworkSlot, Permission, Version};
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// `PrintJSON.type` values (`docs/network protocol.md:188-209`).
///
/// Clients render `data` and may ignore this entirely; an unknown value must
/// degrade to plain text rather than break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PrintJsonType {
    ItemSend,
    ItemCheat,
    Hint,
    Join,
    Part,
    Chat,
    ServerChat,
    Tutorial,
    TagsChanged,
    CommandResult,
    AdminCommandResult,
    Goal,
    Release,
    Collect,
    Countdown,
}

/// Reasons a `Connect` can be refused (`MultiServer.py:1876-1903`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ConnectionRefusedReason {
    InvalidSlot,
    InvalidGame,
    IncompatibleVersion,
    InvalidPassword,
    InvalidItemsHandling,
    /// **Not in the reference's set** — pahoa's own, for an administratively
    /// locked slot.
    ///
    /// The protocol has no reason for this and the list is closed, so the
    /// choice was between lying and inventing. It is always sent *alongside*
    /// `InvalidSlot`, never alone, because of how clients read the list:
    /// `CommonClient.py:981` matches `InvalidSlot` first and stops cleanly,
    /// while an unrecognized reason on its own falls through to
    /// `raise Exception("Unknown connection errors: …")` and then reconnects on
    /// a doubling delay — so a locked player would retry forever.
    ///
    /// Ordering it first would be worse than useless for the same reason. The
    /// cost of the pairing is that a stock client tells a locked player their
    /// slot name is invalid; anything rendering the raw list sees the truth,
    /// and so does the room's own log.
    SlotLocked,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "cmd")]
pub enum ServerPacket {
    RoomInfo(RoomInfo),
    ConnectionRefused(ConnectionRefused),
    Connected(Box<Connected>),
    ReceivedItems(ReceivedItems),
    LocationInfo(LocationInfo),
    RoomUpdate(Box<RoomUpdate>),
    PrintJSON(PrintJson),
    DataPackage(DataPackage),
    InvalidPacket(InvalidPacket),

    /// `Retrieved`, `SetReply` and `Bounced`.
    ///
    /// Archipelago does not construct these: it takes the client's request
    /// object, overwrites `cmd` in place — which keeps `cmd` in its original
    /// position — appends a few fields, and broadcasts the whole thing
    /// including any keys the client invented (`MultiServer.py:2167-2194`,
    /// `:2149-2160`).
    ///
    /// Modelling that with a typed struct plus `#[serde(flatten)]` gets the key
    /// order wrong, because serde emits flattened entries after named ones. So
    /// these stay a raw map and are mutated exactly as Python mutates them;
    /// with serde_json's `preserve_order` the bytes then match for free.
    #[serde(untagged)]
    Echo(Map<String, Value>),
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomInfo {
    pub version: Version,
    pub generator_version: Version,
    pub tags: Vec<String>,
    pub password: bool,
    pub permissions: BTreeMap<String, Permission>,
    /// A *percentage* of the slot's total locations, not an absolute cost.
    pub hint_cost: u32,
    pub location_check_points: u32,
    pub games: Vec<String>,
    /// Omits games with no checksum, matching `MultiServer.py:934-935`.
    pub datapackage_checksums: BTreeMap<String, String>,
    pub seed_name: String,
    /// Unix time, so clients can offset DeathLink timestamps.
    pub time: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionRefused {
    pub errors: Vec<ConnectionRefusedReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Connected {
    pub team: u32,
    pub slot: u32,
    pub players: Vec<NetworkPlayer>,
    pub missing_locations: Vec<i64>,
    pub checked_locations: Vec<i64>,
    pub slot_info: BTreeMap<String, NetworkSlot>,
    pub hint_points: i64,
    /// Omitted entirely when the client sent `slot_data: false`.
    ///
    /// Raw JSON rather than a `Value`: world-supplied slot data can contain
    /// integers wider than `u64` (a live seed has one), which `Value` cannot
    /// represent without enabling `arbitrary_precision` for all JSON in the
    /// server. Emitting the bytes verbatim keeps the digits exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_data: Option<Box<RawValue>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceivedItems {
    /// Absolute offset into the receiver's item list. Zero means "this is your
    /// whole inventory, discard what you had".
    pub index: usize,
    pub items: Vec<NetworkItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocationInfo {
    /// Note `NetworkItem.player` here is the *receiving* player, inverted from
    /// its meaning everywhere else (`NetUtils.py:93-94`).
    pub locations: Vec<NetworkItem>,
}

/// Always partial: only changed fields are sent.
///
/// `checked_locations` is incremental when it comes from a location check but
/// complete when it comes from `update_checked_locations` — same field name,
/// two meanings, and clients are expected to union rather than replace.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RoomUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub players: Option<Vec<NetworkPlayer>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_locations: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_points: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<BTreeMap<String, Permission>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_cost: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_check_points: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PrintJson {
    pub data: Vec<JsonMessagePart>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub print_type: Option<PrintJsonType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiving: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<NetworkItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub countdown: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataPackage {
    /// Pre-rendered `{"games": {…}}`.
    ///
    /// Held rendered and shared rather than typed, because this is the one
    /// reply whose size comes from the *seed* rather than from anything a
    /// client did — 1.1 MiB on a 35-game seed — and `GetDataPackage` is one of
    /// the two packets accepted **before authentication**. Building it per
    /// request cost the actor 5.5 ms, which one socket in a loop turns into a
    /// room-wide stall, and a 6000-client reconnect storm turns into half a
    /// minute of it. Shared, a reply is a refcount bump and one copy.
    ///
    /// [`DataPackageContents`] is how it gets built; see its `render`.
    #[serde(serialize_with = "serialize_shared_raw")]
    pub data: std::sync::Arc<RawValue>,
}

/// Serialize through the `Arc` rather than enabling serde's `rc` feature, which
/// would silently change how every `Arc` in the dependency tree serializes for
/// the sake of one field.
fn serialize_shared_raw<S: serde::Serializer>(
    value: &std::sync::Arc<RawValue>,
    s: S,
) -> Result<S::Ok, S::Error> {
    (**value).serialize(s)
}

/// The typed shape of [`DataPackage::data`], used to produce it.
#[derive(Debug, Clone, Serialize)]
pub struct DataPackageContents {
    pub games: BTreeMap<String, GameData>,
}

impl DataPackageContents {
    /// Render once, for sharing across every reply.
    ///
    /// Going through the same serializer as any other packet is what keeps the
    /// bytes identical to the typed path, so the byte-exact vectors still
    /// describe what goes on the wire.
    pub fn render(&self) -> std::sync::Arc<RawValue> {
        let json = serde_json::to_string(self).expect("a data package always serializes");
        std::sync::Arc::from(RawValue::from_string(json).expect("serde emits valid JSON"))
    }
}

/// What clients cache, keyed by `checksum`.
///
/// Name *groups* are deliberately absent: they are stripped from the served
/// package and exposed through the `_read_item_name_groups_*` data-storage keys
/// instead (`WebHostLib/customserver.py:286-292`).
#[derive(Debug, Clone, Serialize)]
pub struct GameData {
    pub item_name_to_id: BTreeMap<String, i64>,
    pub location_name_to_id: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvalidPacket {
    /// `"cmd"` when the command itself was unknown, `"arguments"` when its
    /// contents were wrong. Extensible by design.
    #[serde(rename = "type")]
    pub problem_type: String,
    pub original_cmd: Option<String>,
    pub text: String,
}

impl ServerPacket {
    /// Build a `Retrieved`/`SetReply`/`Bounced` from the request that caused it.
    ///
    /// Mirrors Python exactly: overwrite `cmd` in place so it keeps its original
    /// position, then append the new fields in order. Everything else the client
    /// sent rides along untouched.
    pub fn echo(mut request: Map<String, Value>, cmd: &str, extra: &[(&str, Value)]) -> Self {
        request.insert("cmd".to_string(), Value::String(cmd.to_string()));
        for (k, v) in extra {
            request.insert((*k).to_string(), v.clone());
        }
        ServerPacket::Echo(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Hint, HintStatus};

    fn json(p: &ServerPacket) -> String {
        serde_json::to_string(p).unwrap()
    }

    #[test]
    fn cmd_comes_first_in_tagged_packets() {
        let p = ServerPacket::LocationInfo(LocationInfo { locations: vec![] });
        assert_eq!(json(&p), r#"{"cmd":"LocationInfo","locations":[]}"#);
    }

    #[test]
    fn room_update_omits_unchanged_fields() {
        // Sending nulls would tell clients things changed to nothing.
        let p = ServerPacket::RoomUpdate(Box::new(RoomUpdate {
            hint_points: Some(12),
            ..Default::default()
        }));
        assert_eq!(json(&p), r#"{"cmd":"RoomUpdate","hint_points":12}"#);
    }

    #[test]
    fn connected_omits_slot_data_when_not_requested() {
        let base = Connected {
            team: 0,
            slot: 1,
            players: vec![],
            missing_locations: vec![],
            checked_locations: vec![],
            slot_info: BTreeMap::new(),
            hint_points: 0,
            slot_data: None,
        };
        let without = json(&ServerPacket::Connected(Box::new(base.clone())));
        assert!(!without.contains("slot_data"), "{without}");

        let with = json(&ServerPacket::Connected(Box::new(Connected {
            slot_data: Some(serde_json::value::to_raw_value(&serde_json::json!({"a": 1})).unwrap()),
            ..base
        })));
        assert!(with.contains(r#""slot_data":{"a":1}"#), "{with}");
    }

    #[test]
    fn echo_keeps_cmd_in_place_and_preserves_unknown_keys() {
        // Python overwrites args["cmd"] in the existing dict, so cmd stays where
        // the client put it, and any extra keys the client invented survive.
        let mut req = Map::new();
        req.insert("cmd".into(), Value::String("Get".into()));
        req.insert("keys".into(), serde_json::json!(["a"]));
        req.insert("my_tag".into(), serde_json::json!(42));

        let p = ServerPacket::echo(req, "Retrieved", &[("keys", serde_json::json!({"a": 1}))]);
        assert_eq!(
            json(&p),
            r#"{"cmd":"Retrieved","keys":{"a":1},"my_tag":42}"#
        );
    }

    #[test]
    fn echo_appends_new_fields_after_existing_ones() {
        let mut req = Map::new();
        req.insert("cmd".into(), Value::String("Set".into()));
        req.insert("key".into(), Value::String("k".into()));

        let p = ServerPacket::echo(
            req,
            "SetReply",
            &[
                ("original_value", serde_json::json!(1)),
                ("value", serde_json::json!(2)),
                ("slot", serde_json::json!(3)),
            ],
        );
        assert_eq!(
            json(&p),
            r#"{"cmd":"SetReply","key":"k","original_value":1,"value":2,"slot":3}"#
        );
    }

    #[test]
    fn print_json_carries_only_the_extras_it_needs() {
        let p = ServerPacket::PrintJSON(PrintJson {
            data: vec![JsonMessagePart::text("hello")],
            print_type: Some(PrintJsonType::Chat),
            slot: Some(2),
            message: Some("hello".into()),
            ..Default::default()
        });
        assert_eq!(
            json(&p),
            r#"{"cmd":"PrintJSON","data":[{"text":"hello"}],"type":"Chat","slot":2,"message":"hello"}"#
        );
    }

    #[test]
    fn hints_serialise_with_their_status_as_an_integer() {
        let h = Hint {
            receiving_player: 1,
            finding_player: 2,
            location: 3,
            item: 4,
            found: false,
            entrance: String::new(),
            item_flags: 1,
            status: HintStatus::Priority,
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.ends_with(r#""status":30,"class":"Hint"}"#), "{s}");
    }

    #[test]
    fn connection_refused_reasons_are_bare_strings() {
        let p = ServerPacket::ConnectionRefused(ConnectionRefused {
            errors: vec![
                ConnectionRefusedReason::InvalidSlot,
                ConnectionRefusedReason::InvalidPassword,
            ],
        });
        assert_eq!(
            json(&p),
            r#"{"cmd":"ConnectionRefused","errors":["InvalidSlot","InvalidPassword"]}"#
        );
    }

    #[test]
    fn data_package_omits_checksums_that_do_not_exist() {
        // Pre-0.3.9 seeds have none, and emitting null would break cache keying.
        let p = ServerPacket::DataPackage(DataPackage {
            data: DataPackageContents {
                games: BTreeMap::from([(
                    "G".to_string(),
                    GameData {
                        item_name_to_id: BTreeMap::from([("Sword".to_string(), 1)]),
                        location_name_to_id: BTreeMap::new(),
                        checksum: None,
                    },
                )]),
            }
            .render(),
        });
        let s = json(&p);
        assert!(!s.contains("checksum"), "{s}");
    }
}
