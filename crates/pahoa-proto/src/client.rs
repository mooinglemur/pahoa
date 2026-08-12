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

use crate::types::Version;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Connect {
    /// Always present in the object, but may be JSON null.
    pub password: Option<String>,
    pub game: Option<String>,
    pub name: String,
    pub uuid: Value,
    pub version: Version,
    pub items_handling: u8,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Defaults to true; `false` omits `slot_data` from `Connected`.
    #[serde(default = "default_true")]
    pub slot_data: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConnectUpdate {
    #[serde(default)]
    pub items_handling: Option<u8>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LocationChecks {
    pub locations: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LocationScouts {
    pub locations: Vec<i64>,
    /// 0 scouts only; 1 also creates a persistent hint; 2 creates hints but
    /// only broadcasts the newly created ones.
    #[serde(default)]
    pub create_as_hint: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreateHints {
    pub locations: Vec<i64>,
    /// Defaults to the requesting slot.
    #[serde(default)]
    pub player: Option<u32>,
    #[serde(default)]
    pub status: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UpdateHint {
    pub player: u32,
    pub location: i64,
    #[serde(default)]
    pub status: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StatusUpdate {
    pub status: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Say {
    pub text: String,
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bounce {
    #[serde(default)]
    pub games: Option<Vec<String>>,
    #[serde(default)]
    pub slots: Option<Vec<u32>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Get {
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Set {
    pub key: String,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub want_reply: bool,
    pub operations: Vec<DataStorageOperation>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DataStorageOperation {
    pub operation: String,
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SetNotify {
    pub keys: Vec<String>,
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
