//! Values that cross the wire, and how they are tagged.
//!
//! Archipelago's encoder walks the object graph and rewrites every NamedTuple
//! into an object carrying an extra `"class"` key naming its Python type
//! (`NetUtils.py:98-107`). So `NetworkItem` goes out as
//! `{"item":…,"location":…,"player":…,"flags":…,"class":"NetworkItem"}` — and
//! field order matters, because the tag is appended last by `_asdict()` plus a
//! `data["class"] = …` assignment.
//!
//! Decoding is far narrower: only `NetworkPlayer`, `NetworkItem` and
//! `NetworkSlot` are in the allowlist, plus a `Version` hook
//! (`NetUtils.py:147-170`). In practice the only tagged value a *server* ever
//! reads from a client is `Version`, so that is the only reconstruction here.

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use std::fmt;

pub use pahoa_multidata::{ClientStatus, HintStatus, SlotType};

/// `(major, minor, build)`.
///
/// Custom clients must tag this `{"class":"Version"}` for the server to compare
/// it. Decoding is deliberately lenient — see [`Version`]'s `Deserialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, build: u32) -> Self {
        Self {
            major,
            minor,
            build,
        }
    }
}

impl From<pahoa_multidata::Version> for Version {
    fn from(v: pahoa_multidata::Version) -> Self {
        Self {
            major: v.major,
            minor: v.minor,
            build: v.build,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.build)
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(4))?;
        m.serialize_entry("major", &self.major)?;
        m.serialize_entry("minor", &self.minor)?;
        m.serialize_entry("build", &self.build)?;
        m.serialize_entry("class", "Version")?;
        m.end()
    }
}

impl<'de> Deserialize<'de> for Version {
    /// Accepts what `NetUtils.get_any_version` accepts (`NetUtils.py:142-144`):
    /// keys are lowercased before lookup, so .NET clients sending
    /// `{"Major":…,"Minor":…,"Build":…}` work, and each value goes through
    /// `int(...)`, which also accepts a numeric string.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Version;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a version object with major, minor and build")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Version, M::Error> {
                let (mut major, mut minor, mut build) = (None, None, None);
                while let Some(key) = map.next_key::<String>()? {
                    // Numbers or numeric strings, matching Python's int().
                    let slot = match key.to_ascii_lowercase().as_str() {
                        "major" => &mut major,
                        "minor" => &mut minor,
                        "build" => &mut build,
                        // Ignore "class" and anything else a client adds.
                        _ => {
                            map.next_value::<de::IgnoredAny>()?;
                            continue;
                        }
                    };
                    *slot = Some(match map.next_value::<serde_json::Value>()? {
                        serde_json::Value::Number(n) => n
                            .as_u64()
                            .ok_or_else(|| de::Error::custom("version component out of range"))?
                            as u32,
                        serde_json::Value::String(s) => {
                            s.trim().parse::<u32>().map_err(de::Error::custom)?
                        }
                        other => {
                            return Err(de::Error::custom(format!(
                                "version component must be a number, got {other}"
                            )));
                        }
                    });
                }
                Ok(Version {
                    major: major.ok_or_else(|| de::Error::missing_field("major"))?,
                    minor: minor.ok_or_else(|| de::Error::missing_field("minor"))?,
                    build: build.ok_or_else(|| de::Error::missing_field("build"))?,
                })
            }
        }

        d.deserialize_map(V)
    }
}

/// `NetUtils.NetworkPlayer(team, slot, alias, name)`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NetworkPlayer {
    pub team: u32,
    pub slot: u32,
    /// Display name; defaults to `name` until the player runs `!alias`.
    pub alias: String,
    /// The immutable slot name from the seed.
    pub name: String,
}

impl Serialize for NetworkPlayer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("team", &self.team)?;
        m.serialize_entry("slot", &self.slot)?;
        m.serialize_entry("alias", &self.alias)?;
        m.serialize_entry("name", &self.name)?;
        m.serialize_entry("class", "NetworkPlayer")?;
        m.end()
    }
}

/// `NetUtils.NetworkItem(item, location, player, flags)`.
///
/// `player` is the **sending** player everywhere except in `LocationInfo`,
/// where it is the receiving player (`NetUtils.py:93-94`). The type cannot
/// encode that inversion; the call site has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct NetworkItem {
    pub item: i64,
    pub location: i64,
    pub player: u32,
    #[serde(default)]
    pub flags: u32,
}

impl Serialize for NetworkItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("item", &self.item)?;
        m.serialize_entry("location", &self.location)?;
        m.serialize_entry("player", &self.player)?;
        m.serialize_entry("flags", &self.flags)?;
        m.serialize_entry("class", "NetworkItem")?;
        m.end()
    }
}

/// `NetUtils.NetworkSlot(name, game, type, group_members)`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NetworkSlot {
    pub name: String,
    pub game: String,
    #[serde(rename = "type")]
    pub slot_type: u8,
    #[serde(default)]
    pub group_members: Vec<u32>,
}

impl NetworkSlot {
    pub fn from_multidata(s: &pahoa_multidata::NetworkSlot) -> Self {
        Self {
            name: s.name.clone(),
            game: s.game.clone(),
            slot_type: s.slot_type as u8,
            group_members: s.group_members.clone(),
        }
    }
}

impl Serialize for NetworkSlot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("name", &self.name)?;
        m.serialize_entry("game", &self.game)?;
        m.serialize_entry("type", &self.slot_type)?;
        m.serialize_entry("group_members", &self.group_members)?;
        m.serialize_entry("class", "NetworkSlot")?;
        m.end()
    }
}

/// `NetUtils.Hint`, as sent inside a `PrintJSON` and the `_read_hints_*` key.
///
/// Only ever serialized: `Hint` is not in Python's decode allowlist, so a hint
/// arriving from a client would land as a plain object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub receiving_player: u32,
    pub finding_player: u32,
    pub location: i64,
    pub item: i64,
    pub found: bool,
    pub entrance: String,
    pub item_flags: u32,
    pub status: HintStatus,
}

impl From<&pahoa_multidata::Hint> for Hint {
    fn from(h: &pahoa_multidata::Hint) -> Self {
        Self {
            receiving_player: h.receiving_player,
            finding_player: h.finding_player,
            location: h.location,
            item: h.item,
            found: h.found,
            entrance: h.entrance.clone(),
            item_flags: h.item_flags,
            status: h.status,
        }
    }
}

impl Serialize for Hint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(9))?;
        m.serialize_entry("receiving_player", &self.receiving_player)?;
        m.serialize_entry("finding_player", &self.finding_player)?;
        m.serialize_entry("location", &self.location)?;
        m.serialize_entry("item", &self.item)?;
        m.serialize_entry("found", &self.found)?;
        m.serialize_entry("entrance", &self.entrance)?;
        m.serialize_entry("item_flags", &self.item_flags)?;
        m.serialize_entry("status", &(self.status as u8))?;
        m.serialize_entry("class", "Hint")?;
        m.end()
    }
}

/// One span of a `PrintJSON` message.
///
/// Clients render `data` and may ignore everything else. Unknown `type` values
/// must fall back to plain text — that rule is what lets new part types ship
/// without breaking old clients (`NetUtils.py:280-283`).
///
/// **Field order is not arbitrary.** Archipelago builds these as dict literals
/// in four helpers (`NetUtils.py:359-370`, `:388-390`), and every one of them
/// puts `text` first, then the part-specific keys, then `type` last:
///
/// ```text
/// add_json_text          {"text", "type"?}
/// add_json_item          {"text", "player", "flags", "type"}
/// add_json_location      {"text", "player", "type"}
/// add_json_hint_status   {"text", "hint_status", "type"}
/// ```
///
/// Declaring the fields in that order makes all four byte-identical, since the
/// absent ones are skipped. `color` is never set by the server — clients add it
/// while rendering — so its position is unconstrained.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonMessagePart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Owning player, for item and location parts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<u32>,
    /// Item classification bits, when the part names an item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_status: Option<HintStatus>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub part_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl JsonMessagePart {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: Some(s.into()),
            ..Default::default()
        }
    }

    pub fn typed(part_type: &str, text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            part_type: Some(part_type.to_string()),
            ..Default::default()
        }
    }

    pub fn player_id(slot: u32) -> Self {
        Self::typed("player_id", slot.to_string())
    }

    pub fn item_id(item: i64, owner: u32, flags: u32) -> Self {
        Self {
            player: Some(owner),
            flags: Some(flags),
            ..Self::typed("item_id", item.to_string())
        }
    }

    pub fn location_id(location: i64, owner: u32) -> Self {
        Self {
            player: Some(owner),
            ..Self::typed("location_id", location.to_string())
        }
    }

    /// The trailing `(priority)`/`(found)`/… span of a hint message.
    ///
    /// The text is redundant with `hint_status` and clients that understand the
    /// field re-render it in their own words, but Python sends both
    /// (`NetUtils.py:388-390`) and older clients only read the text.
    pub fn hint_status(status: HintStatus) -> Self {
        Self {
            hint_status: Some(status),
            ..Self::typed("hint_status", status.label())
        }
    }
}

/// `NetUtils.Permission`, an `IntFlag`.
///
/// The protocol doc calls this an `IntEnum`; the code is an `IntFlag`
/// (`NetUtils.py:55`), and `auto` is `goal | 0b100`, so the bits matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", from = "u8")]
pub enum Permission {
    Disabled = 0b000,
    Enabled = 0b001,
    Goal = 0b010,
    Auto = 0b110,
    AutoEnabled = 0b111,
}

impl From<Permission> for u8 {
    fn from(p: Permission) -> u8 {
        p as u8
    }
}

impl From<u8> for Permission {
    fn from(v: u8) -> Self {
        match v {
            0b001 => Self::Enabled,
            0b010 => Self::Goal,
            0b110 => Self::Auto,
            0b111 => Self::AutoEnabled,
            _ => Self::Disabled,
        }
    }
}

impl Permission {
    /// The option text this permission came from.
    ///
    /// The reference keeps these modes as *strings* on the context and only
    /// converts to `Permission` for `RoomInfo`, so `!options` prints the word
    /// rather than the number. Round-trips with [`Permission::from_text`] for
    /// the canonical spellings, which is what the option parser accepts.
    pub fn as_text(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Goal => "goal",
            Self::Auto => "auto",
            Self::AutoEnabled => "auto-enabled",
        }
    }

    /// Whether a player may use this at will (`"enabled" in mode`, which is a
    /// substring test in the reference and so also true for `auto-enabled`).
    pub fn allows_manual(self) -> bool {
        self as u8 & Self::Enabled as u8 != 0
    }

    /// Whether reaching the goal unlocks it, or triggers it automatically.
    pub fn on_goal(self) -> bool {
        self as u8 & Self::Goal as u8 != 0
    }

    /// Whether the server does it for the player on goal completion.
    pub fn is_auto(self) -> bool {
        self as u8 & 0b100 != 0
    }

    /// `Permission.from_text` (`NetUtils.py:62-71`).
    pub fn from_text(text: &str) -> Self {
        let t = text.to_ascii_lowercase();
        let mut bits = 0u8;
        if t.contains("auto") {
            bits |= 0b110;
        }
        if t.contains("enabled") {
            bits |= 0b001;
        }
        if t.contains("goal") {
            bits |= 0b010;
        }
        Self::from(bits)
    }
}

/// `items_handling` flags from `Connect` (`MultiServer.py:192-204`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemsHandling(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid items_handling flag combination")]
pub struct InvalidItemsHandling;

impl ItemsHandling {
    pub const REMOTE_ITEMS: u8 = 0b001;
    pub const OWN_WORLD: u8 = 0b010;
    pub const START_INVENTORY: u8 = 0b100;

    /// Rejects the combinations Python rejects: bits 2 and 4 require bit 1, so
    /// `0b110` without `0b001` is an error and becomes `InvalidItemsHandling`
    /// in the `ConnectionRefused` reply.
    pub fn new(bits: u8) -> Result<Self, InvalidItemsHandling> {
        if bits & Self::REMOTE_ITEMS == 0 && bits & 0b110 != 0 {
            return Err(InvalidItemsHandling);
        }
        Ok(Self(bits))
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    /// No `ReceivedItems` at all.
    pub fn no_items(self) -> bool {
        self.0 & Self::REMOTE_ITEMS == 0
    }

    pub fn remote_items(self) -> bool {
        self.0 & Self::OWN_WORLD != 0
    }

    pub fn remote_start_inventory(self) -> bool {
        self.0 & Self::START_INVENTORY != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(v: &impl Serialize) -> String {
        serde_json::to_string(v).unwrap()
    }

    #[test]
    fn tagged_values_end_with_their_class() {
        // Field order is observable, and Python appends "class" last.
        assert_eq!(
            json(&NetworkItem {
                item: 1,
                location: 2,
                player: 3,
                flags: 4
            }),
            r#"{"item":1,"location":2,"player":3,"flags":4,"class":"NetworkItem"}"#
        );
        assert_eq!(
            json(&Version::new(0, 6, 8)),
            r#"{"major":0,"minor":6,"build":8,"class":"Version"}"#
        );
    }

    #[test]
    fn version_accepts_dotnet_capitalisation() {
        let v: Version =
            serde_json::from_str(r#"{"Major":1,"Minor":6,"Build":8,"class":"Version"}"#).unwrap();
        assert_eq!(v, Version::new(1, 6, 8));
    }

    #[test]
    fn version_accepts_numeric_strings() {
        // Python runs each component through int(), which parses strings.
        let v: Version = serde_json::from_str(r#"{"major":"0","minor":"5","build":"1"}"#).unwrap();
        assert_eq!(v, Version::new(0, 5, 1));
    }

    #[test]
    fn version_rejects_nonsense_components() {
        assert!(serde_json::from_str::<Version>(r#"{"major":true,"minor":1,"build":1}"#).is_err());
        assert!(serde_json::from_str::<Version>(r#"{"minor":1,"build":1}"#).is_err());
    }

    #[test]
    fn versions_order_by_component() {
        assert!(Version::new(0, 5, 0) < Version::new(0, 6, 8));
        assert!(Version::new(0, 6, 2) < Version::new(0, 6, 10));
    }

    #[test]
    fn json_message_part_omits_absent_fields() {
        // Clients switch on presence, so emitting nulls would change meaning.
        assert_eq!(json(&JsonMessagePart::text("hi")), r#"{"text":"hi"}"#);
        assert_eq!(
            json(&JsonMessagePart::item_id(5, 2, 0b001)),
            r#"{"text":"5","player":2,"flags":1,"type":"item_id"}"#
        );
    }

    #[test]
    fn json_message_parts_key_order_matches_each_python_builder() {
        // `type` last for the parts that carry extra keys, second for the ones
        // that do not — see the note on JsonMessagePart. Pinned against the real
        // functions by crates/pahoa-room/tests/message_vectors.jsonl.
        assert_eq!(
            json(&JsonMessagePart::player_id(3)),
            r#"{"text":"3","type":"player_id"}"#
        );
        assert_eq!(
            json(&JsonMessagePart::location_id(1234, 1)),
            r#"{"text":"1234","player":1,"type":"location_id"}"#
        );
        assert_eq!(
            json(&JsonMessagePart::hint_status(HintStatus::Priority)),
            r#"{"text":"(priority)","hint_status":30,"type":"hint_status"}"#
        );
    }

    #[test]
    fn permission_parses_archipelago_option_text() {
        assert_eq!(Permission::from_text("disabled"), Permission::Disabled);
        assert_eq!(Permission::from_text("enabled"), Permission::Enabled);
        assert_eq!(Permission::from_text("goal"), Permission::Goal);
        assert_eq!(Permission::from_text("auto"), Permission::Auto);
        // "auto-enabled" sets the auto bits and the manual bit.
        assert_eq!(
            Permission::from_text("auto-enabled"),
            Permission::AutoEnabled
        );
        assert_eq!(
            Permission::from_text("auto_enabled"),
            Permission::AutoEnabled
        );
    }

    #[test]
    fn permission_serializes_as_its_integer_value() {
        assert_eq!(json(&Permission::AutoEnabled), "7");
        assert_eq!(json(&Permission::Auto), "6");
        assert_eq!(json(&Permission::Disabled), "0");
    }

    #[test]
    fn items_handling_rejects_dependent_bits_without_the_base() {
        // 0b010 and 0b100 both require 0b001.
        assert!(ItemsHandling::new(0b010).is_err());
        assert!(ItemsHandling::new(0b100).is_err());
        assert!(ItemsHandling::new(0b110).is_err());

        assert!(
            ItemsHandling::new(0b000).is_ok(),
            "no items at all is valid"
        );
        assert!(ItemsHandling::new(0b111).is_ok());
    }

    #[test]
    fn items_handling_exposes_the_three_behaviors() {
        let none = ItemsHandling::new(0b000).unwrap();
        assert!(none.no_items());

        let all = ItemsHandling::new(0b111).unwrap();
        assert!(!all.no_items());
        assert!(all.remote_items());
        assert!(all.remote_start_inventory());

        let basic = ItemsHandling::new(0b001).unwrap();
        assert!(!basic.no_items());
        assert!(!basic.remote_items());
        assert!(!basic.remote_start_inventory());
    }
}
