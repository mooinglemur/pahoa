//! Packets the server receives.
//!
//! Decoding keeps the original map alongside the typed form for the three
//! commands whose replies echo the request back (`Get`, `Set`, `Bounce`), since
//! reproducing Archipelago's key order means mutating the client's own object
//! rather than rebuilding one.
//!
//! Note what is *not* validated here. Archipelago checks only `password` and
//! `game` for presence on `Connect` and then indexes the rest unguarded
//! (`MultiServer.py:1870-1871`), so a missing `name` or `version` raises and
//! drops the socket instead of returning `InvalidPacket`. That behavior is
//! reproduced at the room layer, where the strict/lenient switch lives; this
//! layer reports a decode failure and lets the caller decide.
//!
//! **Which failures reach the room at all is decided by [`crate::Arg`].** A
//! field the reference guards with an `if` is wrapped in one, so a wrong type
//! arrives as a value the handler can answer `InvalidPacket` to; a field it
//! indexes unguarded is left bare, so a wrong type fails here and the socket
//! closes as Python's would. See that module for why the split cannot live in
//! this layer.

use crate::Arg;
use crate::lenient;
use crate::types::Version;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Ids as the raw-list fields hold them, for callers that have real integers.
///
/// `locations` on three of the commands is a `Vec<Value>` because the
/// reference inspects it element by element and reacts differently to each
/// kind of junk; a Rust caller building a packet has none of that problem.
pub fn ids(ids: impl IntoIterator<Item = i64>) -> Vec<Value> {
    ids.into_iter().map(Value::from).collect()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Connect {
    /// Must be present, and a string or null: the reference tests its type
    /// explicitly and answers `InvalidPacket` (`MultiServer.py:1904-1907`).
    #[serde(default)]
    pub password: Arg<Option<String>>,
    /// Must be present; any type. A non-string simply never equals the slot's
    /// game, which is `InvalidGame`, not a disconnect.
    #[serde(default)]
    pub game: Arg<Option<String>>,
    /// Required — `args['name']` is indexed unguarded, so an absent one raises.
    /// A *present* name of the wrong type merely matches no slot, giving
    /// `InvalidSlot`.
    pub name: Arg<String>,
    pub uuid: Value,
    pub version: Version,
    /// Required; a wrong type reaches the reference's `items_handling` setter,
    /// whose `value & 0b001` raises `TypeError` into the same `except` that
    /// produces `InvalidItemsHandling` (`MultiServer.py:1927-1930`).
    pub items_handling: Arg<lenient::U8>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Defaults to true; anything falsy omits `slot_data` from `Connected`.
    ///
    /// The reference asks `if args.get("slot_data", True):`
    /// (`MultiServer.py:1973`) — a truth test, so an explicit `null` here means
    /// *no* slot data while omitting the key means yes. `default` fires only on
    /// absence, which keeps the two apart.
    #[serde(default = "default_true")]
    #[serde(deserialize_with = "crate::lenient::bool_")]
    pub slot_data: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConnectUpdate {
    /// Absent *or null* means "leave it alone"
    /// (`args.get('items_handling', None) is not None`); a wrong type is
    /// answered rather than fatal, as on `Connect`.
    #[serde(default)]
    pub items_handling: Arg<Option<lenient::U8>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LocationChecks {
    /// Raw, because the reference does not check these at all: it hands the
    /// list to `register_location_checks`, which intersects it with the slot's
    /// location set (`MultiServer.py:2042-2045`). A junk entry matches nothing
    /// and is silently ignored — it is not worth a disconnect, and a client
    /// with one bad id in a batch still gets the rest of its checks.
    ///
    /// A non-list still fails here, because iterating one raises in Python too.
    pub locations: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LocationScouts {
    /// Raw, because this one *is* checked, element by element, with its own
    /// message (`MultiServer.py:2052-2058`).
    pub locations: Vec<Value>,
    /// 0 scouts only; 1 also creates a persistent hint; 2 creates hints but
    /// only broadcasts the newly created ones.
    #[serde(default)]
    #[serde(deserialize_with = "crate::lenient::i64_")]
    pub create_as_hint: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreateHints {
    /// Raw: an id that is not a location of the named player is reported as
    /// such (`MultiServer.py:2091-2097`), not as a decode failure.
    pub locations: Vec<Value>,
    /// Defaults to the requesting slot.
    #[serde(default)]
    #[serde(deserialize_with = "crate::lenient::opt_u32")]
    pub player: Option<u32>,
    /// A value outside `HintStatus` raises `ValueError` into an
    /// `InvalidPacket{text: "Unknown Status: …"}` (`MultiServer.py:2078-2084`),
    /// and so does one of the wrong type.
    #[serde(default)]
    pub status: Arg<Option<lenient::I64>>,
}

/// All three are required — the reference indexes them, so an absent one
/// raises — but a *present* one of the wrong type is checked with
/// `isinstance` and answered `InvalidPacket{text: "UpdateHint"}`
/// (`MultiServer.py:2126-2131`). `Arg` without `#[serde(default)]` is exactly
/// that pair of rules.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UpdateHint {
    /// The **finding** player, not the receiver — hints are looked up by where
    /// the item sits (`MultiServer.py:2097`).
    pub player: Arg<lenient::U32>,
    pub location: Arg<lenient::I64>,
    /// Nullable: an explicit `null` means "leave the status alone" and is
    /// ignored, which is why the inner type is an `Option`.
    pub status: Arg<Option<lenient::I64>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StatusUpdate {
    #[serde(deserialize_with = "crate::lenient::i64_")]
    pub status: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Say {
    /// Absent, not a string, or not printable are one guard with one answer
    /// (`MultiServer.py:2176-2180`).
    #[serde(default)]
    pub text: Arg<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GetDataPackage {
    #[serde(default)]
    pub games: Option<Vec<String>>,
    /// Undocumented, past its own removal TODO, and still honored by the
    /// reference server (`MultiServer.py:1943`, `:1950-1957`).
    #[serde(default)]
    pub exclusions: Option<Vec<String>>,
}

/// Each filter is validated as a whole — not a list, or any element of the
/// wrong type, produces the same `InvalidPacket` with its own text
/// (`MultiServer.py:2185-2211`). One `Arg` per filter says exactly that, since
/// the reference draws no line between the two failures.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bounce {
    #[serde(default)]
    pub games: Arg<Vec<String>>,
    #[serde(default)]
    pub slots: Arg<Vec<u32>>,
    #[serde(default)]
    pub tags: Arg<Vec<String>>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Get {
    /// Raw elements: the list itself is guarded (`"keys" not in args or
    /// type(args["keys"]) != list`), while a non-string *inside* it reaches
    /// `key.startswith("_read_")` and raises (`MultiServer.py:2246-2257`).
    #[serde(default)]
    pub keys: Arg<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Set {
    /// Absent is answered; present-but-not-a-string reaches
    /// `args["key"].startswith(...)` and raises (`MultiServer.py:2261`).
    #[serde(default)]
    pub key: Arg<String>,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    #[serde(deserialize_with = "crate::lenient::bool_")]
    pub want_reply: bool,
    /// Absent or not a list is answered; a malformed *entry* reaches
    /// `operation["operation"]` and raises (`MultiServer.py:2269-2270`), so the
    /// entries stay raw and the handler makes that call.
    #[serde(default)]
    pub operations: Arg<Vec<Value>>,
}

/// One entry of `Set.operations`, for a caller that wants it typed.
///
/// `Set` itself keeps them raw — a malformed entry has to reach the handler so
/// it can close the socket the way `operation["operation"]` raising does — but
/// the shape is part of the protocol and worth naming.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DataStorageOperation {
    pub operation: String,
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SetNotify {
    /// As `Get`, minus the raise: a non-string key is merely stored under
    /// something no `Set` can ever name, since `Set.key` must be a string to
    /// survive its own `startswith`.
    #[serde(default)]
    pub keys: Arg<Vec<Value>>,
}

/// A decoded client command.
///
/// `Get`, `Set` and `Bounce` carry the raw request map too, because their
/// replies are that map with `cmd` rewritten and fields appended.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientPacket {
    Connect(Box<Connect>),
    ConnectUpdate(ConnectUpdate),
    Sync,
    LocationChecks(LocationChecks),
    LocationScouts(LocationScouts),
    CreateHints(CreateHints),
    UpdateHint(UpdateHint),
    StatusUpdate(StatusUpdate),
    Say(Say),
    GetDataPackage(GetDataPackage),
    Bounce(Bounce, Map<String, Value>),
    Get(Get, Map<String, Value>),
    Set(Box<Set>, Map<String, Value>),
    SetNotify(SetNotify),
}

impl ClientPacket {
    /// The command name, for logging and `InvalidPacket.original_cmd`.
    pub fn cmd(&self) -> &'static str {
        match self {
            Self::Connect(_) => "Connect",
            Self::ConnectUpdate(_) => "ConnectUpdate",
            Self::Sync => "Sync",
            Self::LocationChecks(_) => "LocationChecks",
            Self::LocationScouts(_) => "LocationScouts",
            Self::CreateHints(_) => "CreateHints",
            Self::UpdateHint(_) => "UpdateHint",
            Self::StatusUpdate(_) => "StatusUpdate",
            Self::Say(_) => "Say",
            Self::GetDataPackage(_) => "GetDataPackage",
            Self::Bounce(..) => "Bounce",
            Self::Get(..) => "Get",
            Self::Set(..) => "Set",
            Self::SetNotify(_) => "SetNotify",
        }
    }

    /// Whether this command is accepted before `Connect` succeeds.
    ///
    /// Everything else falls through Python's `elif client.auth:` chain and is
    /// silently ignored rather than refused (`MultiServer.py:1963`).
    pub fn allowed_before_auth(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::GetDataPackage(_))
    }
}
