//! Fields whose type the *room* checks, because the reference does.
//!
//! # Why decoding must not be the judge
//!
//! `process_client_cmd` handles a wrongly-typed argument in one of two ways,
//! and which one it picks is a property of the individual field, not of the
//! command:
//!
//! - **It answers `InvalidPacket` and reads the next frame.** `Say` with a
//!   non-string `text`, `Get` with a `keys` that is not a list, `Bounce` with a
//!   malformed filter, `Connect` with a `password` that is neither a string nor
//!   null — each is a guarded `if` with its own message
//!   (`MultiServer.py:1904-1907`, `:2176-2180`, `:2246-2250`, `:2185-2211`).
//! - **It raises, and the socket dies.** Anything indexed unguarded —
//!   `args['name']`, `args['version']`, `args["locations"]` — throws `KeyError`
//!   or `TypeError` out of the read loop (`MultiServer.py:900-917`).
//!
//! A `#[derive(Deserialize)]` field can only produce the second. `serde` fails
//! the whole packet before the room ever sees it, so a client that sends
//! `"password": 0` got its connection closed where Archipelago would have told
//! it what was wrong and carried on. That is not a stricter server; it is a
//! server that hangs up on a fixable mistake, and the client cannot even read
//! the reason because the reason was never sent.
//!
//! # How this splits the two
//!
//! [`Arg<T>`] never fails to deserialize. A value of the wrong type arrives as
//! [`Arg::Wrong`] and a `#[serde(default)]` absence as [`Arg::Missing`], so the
//! handler decides — next to the reference's own guard, which is already there
//! for the values that did parse.
//!
//! **Leaving `Arg` off a field is therefore the way to say "the reference
//! raises here".** `UpdateHint.player` is `Arg<i64>` with no `default`: a wrong
//! type is answerable, an absent key is not, and the two stay apart because
//! serde only calls the deserializer when the key is present.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Arg<T> {
    /// The key was absent. Only reachable with `#[serde(default)]`; without it
    /// an absent key is a decode failure, which drops the connection.
    #[default]
    Missing,
    /// Present, and not the type the protocol says.
    Wrong,
    Ok(T),
}

impl<T> Arg<T> {
    pub fn ok(self) -> Option<T> {
        match self {
            Self::Ok(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_ok(&self) -> Option<&T> {
        match self {
            Self::Ok(v) => Some(v),
            _ => None,
        }
    }

    /// Absent *or* the wrong type — the shape of a guard written
    /// `"keys" not in args or type(args["keys"]) != list`, which is most of
    /// them.
    pub fn is_bad(&self) -> bool {
        !matches!(self, Self::Ok(_))
    }

    /// The wrong type only, for a field the reference defaults instead of
    /// requiring: `args.get("games", [])` is happy with an absent key and
    /// refuses a malformed one.
    pub fn is_wrong(&self) -> bool {
        matches!(self, Self::Wrong)
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Arg<T> {
    /// Buffers through [`Value`] so a failed `T` can be swallowed.
    ///
    /// `decode` already materializes the frame as a `Value` before dispatching
    /// on `cmd`, so this re-walks a tree that is in memory either way rather
    /// than costing a second parse.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(match T::deserialize(v) {
            Ok(t) => Self::Ok(t),
            Err(_) => Self::Wrong,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Holder {
        #[serde(default)]
        maybe: Arg<String>,
        required: Arg<u32>,
    }

    #[test]
    fn a_wrong_type_is_reported_rather_than_failing_the_packet() {
        let h: Holder = serde_json::from_str(r#"{"maybe":7,"required":3}"#).unwrap();
        assert_eq!(h.maybe, Arg::Wrong);
        assert_eq!(h.required, Arg::Ok(3));
    }

    #[test]
    fn absence_and_a_wrong_type_stay_distinct() {
        let h: Holder = serde_json::from_str(r#"{"required":3}"#).unwrap();
        assert_eq!(h.maybe, Arg::Missing);
        assert!(h.maybe.is_bad());
    }

    /// Without `#[serde(default)]` an absent key must still fail, because that
    /// is how a handler says "the reference raises here".
    #[test]
    fn a_required_arg_is_still_required() {
        assert!(serde_json::from_str::<Holder>(r#"{"maybe":"x"}"#).is_err());
    }

    /// A null is a value, not an absence: `Arg<Option<T>>` accepts it, and a
    /// bare `Arg<T>` calls it wrong. The reference draws the same line —
    /// `type(args['password']) not in [str, NoneType]` admits null, while
    /// `args['name']` merely fails to match any slot.
    #[test]
    fn null_is_a_value() {
        let h: Holder = serde_json::from_str(r#"{"maybe":null,"required":3}"#).unwrap();
        assert_eq!(h.maybe, Arg::Wrong);

        #[derive(Deserialize)]
        struct Nullable {
            v: Arg<Option<String>>,
        }
        let n: Nullable = serde_json::from_str(r#"{"v":null}"#).unwrap();
        assert_eq!(n.v, Arg::Ok(None));
    }
}
