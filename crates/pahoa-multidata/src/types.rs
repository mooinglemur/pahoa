//! The value types Archipelago pickles into multidata and save files.
//!
//! Field order matches the Python namedtuples exactly, because they are decoded
//! positionally from `NEWOBJ` argument tuples.

use crate::error::{Error, Path, Result};
use crate::extract::Extract;
use pahoa_pickle::PyObj;
use serde::{Deserialize, Serialize};

/// `NetUtils.SlotType`, an `IntFlag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    Spectator = 0b00,
    Player = 0b01,
    Group = 0b10,
}

impl SlotType {
    pub fn from_i64(v: i64, path: &Path) -> Result<Self> {
        match v {
            0b00 => Ok(Self::Spectator),
            0b01 => Ok(Self::Player),
            0b10 => Ok(Self::Group),
            _ => Err(Error::Enum {
                path: path.clone(),
                name: "SlotType",
                value: v,
            }),
        }
    }

    /// Anything that is not exactly `Player` counts as having reached its goal
    /// the moment the room loads (`NetUtils.py:49-52`), which is how spectators
    /// and item-link groups avoid blocking a team-completion check.
    pub fn always_goal(self) -> bool {
        self != Self::Player
    }
}

/// `NetUtils.ClientStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClientStatus {
    Unknown = 0,
    Connected = 5,
    Ready = 10,
    Playing = 20,
    Goal = 30,
}

impl ClientStatus {
    pub fn from_i64(v: i64, path: &Path) -> Result<Self> {
        Ok(match v {
            0 => Self::Unknown,
            5 => Self::Connected,
            10 => Self::Ready,
            20 => Self::Playing,
            30 => Self::Goal,
            _ => {
                return Err(Error::Enum {
                    path: path.clone(),
                    name: "ClientStatus",
                    value: v,
                });
            }
        })
    }
}

/// `NetUtils.HintStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintStatus {
    Unspecified = 0,
    NoPriority = 10,
    Avoid = 20,
    Priority = 30,
    Found = 40,
}

impl HintStatus {
    pub fn from_i64(v: i64, path: &Path) -> Result<Self> {
        Ok(match v {
            0 => Self::Unspecified,
            10 => Self::NoPriority,
            20 => Self::Avoid,
            30 => Self::Priority,
            40 => Self::Found,
            _ => {
                return Err(Error::Enum {
                    path: path.clone(),
                    name: "HintStatus",
                    value: v,
                });
            }
        })
    }
}

/// These are Python `IntEnum`/`IntFlag` types, so their integer value is the
/// representation — on the wire and in any JSON we emit. serde's default enum
/// encoding would write the variant *name*, which a client would reject and
/// which the protocol vectors catch immediately.
macro_rules! int_enum_serde {
    ($ty:ty, $name:literal) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_u8(*self as u8)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                let v = i64::deserialize(d)?;
                Self::from_i64(v, &Path::root())
                    .map_err(|_| serde::de::Error::custom(format!("{v} is not a valid {}", $name)))
            }
        }
    };
}

int_enum_serde!(SlotType, "SlotType");
int_enum_serde!(ClientStatus, "ClientStatus");
int_enum_serde!(HintStatus, "HintStatus");

/// Item classification bits as they appear on the wire.
///
/// Only the low three bits of `ItemClassification` are transmitted
/// (`BaseClasses.py:1587-1589`); the rest are generation-side concerns.
pub mod item_flags {
    pub const ADVANCEMENT: u32 = 0b001;
    pub const USEFUL: u32 = 0b010;
    pub const TRAP: u32 = 0b100;

    /// The only classification the server itself reads: a trap defaults its
    /// hint to `Avoid` (`MultiServer.py:1209`, `:1250`).
    pub fn is_trap(flags: u32) -> bool {
        flags & TRAP != 0
    }
}

/// `NetUtils.NetworkSlot(name, game, type, group_members)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSlot {
    pub name: String,
    pub game: String,
    #[serde(rename = "type")]
    pub slot_type: SlotType,
    /// Populated only for `SlotType::Group`.
    pub group_members: Vec<u32>,
}

impl NetworkSlot {
    pub fn from_py(v: &PyObj, path: &Path) -> Result<Self> {
        let args = v
            .as_instance_of("NetUtils", "NetworkSlot")
            .ok_or_else(|| Error::Type {
                path: path.clone(),
                expected: "NetUtils.NetworkSlot",
                found: v.type_name(),
            })?;
        if args.len() != 4 {
            return Err(Error::Arity {
                path: path.clone(),
                expected: 4,
                found: args.len(),
            });
        }

        let type_path = path.key("type");
        let raw_type = args[2]
            .as_instance_of("NetUtils", "SlotType")
            .and_then(|a| a.first())
            .ok_or_else(|| Error::Type {
                path: type_path.clone(),
                expected: "NetUtils.SlotType",
                found: args[2].type_name(),
            })?
            .int(&type_path)?;

        let members_path = path.key("group_members");
        let group_members = args[3]
            .seq(&members_path)?
            .iter()
            .enumerate()
            .map(|(i, m)| m.u32_(&members_path.index(i)))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            name: args[0].str_(&path.key("name"))?.to_string(),
            game: args[1].str_(&path.key("game"))?.to_string(),
            slot_type: SlotType::from_i64(raw_type, &type_path)?,
            group_members,
        })
    }
}

/// `NetUtils.NetworkItem(item, location, player, flags)`.
///
/// `player` is the *sending* player everywhere except in `LocationInfo`, where
/// it is the receiving player (`NetUtils.py:93-94`). The type cannot express
/// that inversion; call sites must.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkItem {
    pub item: i64,
    pub location: i64,
    pub player: u32,
    pub flags: u32,
}

/// `NetUtils.Hint`.
///
/// Python's `__hash__` excludes `found`, `item_flags` and `status`
/// (`NetUtils.py:418-419`) so that updating a hint's status replaces it in
/// place rather than adding a duplicate. [`Hint::identity`] exposes that same
/// key; do not derive `Hash` on the whole struct and expect matching behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The subset of fields Python hashes and compares a hint by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HintIdentity {
    pub receiving_player: u32,
    pub finding_player: u32,
    pub location: i64,
    pub item: i64,
    pub entrance: String,
}

impl Hint {
    pub fn identity(&self) -> HintIdentity {
        HintIdentity {
            receiving_player: self.receiving_player,
            finding_player: self.finding_player,
            location: self.location,
            item: self.item,
            entrance: self.entrance.clone(),
        }
    }

    pub fn from_py(v: &PyObj, path: &Path) -> Result<Self> {
        let args = v
            .as_instance_of("NetUtils", "Hint")
            .ok_or_else(|| Error::Type {
                path: path.clone(),
                expected: "NetUtils.Hint",
                found: v.type_name(),
            })?;
        // Older generations predate `status`, so accept 7 or 8 fields.
        if !(7..=8).contains(&args.len()) {
            return Err(Error::Arity {
                path: path.clone(),
                expected: 8,
                found: args.len(),
            });
        }

        let status = match args.get(7) {
            Some(s) => {
                let sp = path.key("status");
                // Normally a HintStatus enum built via REDUCE; accept a bare
                // int too, since that is what a hand-written or older producer
                // would emit and the value is unambiguous either way.
                let raw = match s
                    .as_instance_of("NetUtils", "HintStatus")
                    .and_then(|a| a.first())
                {
                    Some(inner) => inner.int(&sp)?,
                    None => s.int(&sp)?,
                };
                HintStatus::from_i64(raw, &sp)?
            }
            None => HintStatus::Unspecified,
        };

        Ok(Self {
            receiving_player: args[0].u32_(&path.key("receiving_player"))?,
            finding_player: args[1].u32_(&path.key("finding_player"))?,
            location: args[2].int(&path.key("location"))?,
            item: args[3].int(&path.key("item"))?,
            found: args[4].bool_(&path.key("found"))?,
            entrance: args[5].str_(&path.key("entrance"))?.to_string(),
            item_flags: args[6].u32_(&path.key("item_flags"))?,
            status,
        })
    }
}

/// A `(major, minor, build)` version triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

    pub fn from_py(v: &PyObj, path: &Path) -> Result<Self> {
        let parts = v.tuple_n(path, 3)?;
        Ok(Self {
            major: parts[0].u32_(&path.index(0))?,
            minor: parts[1].u32_(&path.index(1))?,
            build: parts[2].u32_(&path.index(2))?,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.build)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot_py(name: &str, game: &str, ty: i64, members: Vec<PyObj>) -> PyObj {
        PyObj::Instance {
            class: pahoa_pickle::ClassId::new("NetUtils", "NetworkSlot"),
            args: vec![
                PyObj::Str(name.into()),
                PyObj::Str(game.into()),
                PyObj::Instance {
                    class: pahoa_pickle::ClassId::new("NetUtils", "SlotType"),
                    args: vec![PyObj::Int(ty)],
                },
                PyObj::Tuple(members),
            ],
        }
    }

    #[test]
    fn decodes_a_player_slot() {
        let s = NetworkSlot::from_py(&slot_py("Alice", "Timespinner", 1, vec![]), &Path::root())
            .unwrap();
        assert_eq!(s.name, "Alice");
        assert_eq!(s.slot_type, SlotType::Player);
        assert!(s.group_members.is_empty());
        assert!(!s.slot_type.always_goal());
    }

    #[test]
    fn decodes_a_group_slot_with_members() {
        let s = slot_py(
            "Item Link",
            "Archipelago",
            2,
            vec![PyObj::Int(1), PyObj::Int(4)],
        );
        let s = NetworkSlot::from_py(&s, &Path::root()).unwrap();
        assert_eq!(s.slot_type, SlotType::Group);
        assert_eq!(s.group_members, [1, 4]);
        // Groups and spectators are goal-complete on load.
        assert!(s.slot_type.always_goal());
    }

    #[test]
    fn spectators_are_goal_complete_but_players_are_not() {
        assert!(SlotType::Spectator.always_goal());
        assert!(SlotType::Group.always_goal());
        assert!(!SlotType::Player.always_goal());
    }

    #[test]
    fn rejects_an_unknown_slot_type_with_context() {
        let err = NetworkSlot::from_py(
            &slot_py("x", "y", 7, vec![]),
            &Path::root().key("slot_info"),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "slot_info.type: 7 is not a valid SlotType");
    }

    fn hint_py(args: Vec<PyObj>) -> PyObj {
        PyObj::Instance {
            class: pahoa_pickle::ClassId::new("NetUtils", "Hint"),
            args,
        }
    }

    #[test]
    fn decodes_a_hint_with_status() {
        let h = hint_py(vec![
            PyObj::Int(1),
            PyObj::Int(2),
            PyObj::Int(100),
            PyObj::Int(200),
            PyObj::Bool(false),
            PyObj::Str("".into()),
            PyObj::Int(0b001),
            PyObj::Instance {
                class: pahoa_pickle::ClassId::new("NetUtils", "HintStatus"),
                args: vec![PyObj::Int(30)],
            },
        ]);
        let h = Hint::from_py(&h, &Path::root()).unwrap();
        assert_eq!(h.status, HintStatus::Priority);
        assert_eq!(h.item_flags, item_flags::ADVANCEMENT);
    }

    #[test]
    fn accepts_pre_status_hints_from_older_generations() {
        let h = hint_py(vec![
            PyObj::Int(1),
            PyObj::Int(2),
            PyObj::Int(100),
            PyObj::Int(200),
            PyObj::Bool(true),
            PyObj::Str("Door".into()),
            PyObj::Int(0),
        ]);
        let h = Hint::from_py(&h, &Path::root()).unwrap();
        assert_eq!(h.status, HintStatus::Unspecified);
        assert_eq!(h.entrance, "Door");
    }

    #[test]
    fn hint_identity_ignores_status_and_found() {
        // Python's Hint.__hash__ excludes these, so a status update replaces
        // the hint in place instead of duplicating it.
        let base = Hint {
            receiving_player: 1,
            finding_player: 2,
            location: 3,
            item: 4,
            found: false,
            entrance: "E".into(),
            item_flags: 0,
            status: HintStatus::Unspecified,
        };
        let updated = Hint {
            found: true,
            item_flags: 4,
            status: HintStatus::Found,
            ..base.clone()
        };
        assert_ne!(base, updated);
        assert_eq!(base.identity(), updated.identity());
    }

    #[test]
    fn trap_flag_drives_the_only_classification_the_server_reads() {
        assert!(item_flags::is_trap(item_flags::TRAP));
        assert!(item_flags::is_trap(
            item_flags::TRAP | item_flags::ADVANCEMENT
        ));
        assert!(!item_flags::is_trap(item_flags::USEFUL));
    }

    #[test]
    fn versions_order_naturally() {
        assert!(Version::new(0, 5, 0) < Version::new(0, 6, 8));
        assert!(Version::new(0, 6, 2) < Version::new(0, 6, 10));
        assert_eq!(Version::new(0, 6, 8).to_string(), "0.6.8");
    }
}
