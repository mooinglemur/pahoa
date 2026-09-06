//! Values a client spells in some other type than the protocol says.
//!
//! Two families so far, and they resolve differently: an integer spelled as a
//! float is accepted only when it is exactly one, while a boolean spelled as
//! anything at all is accepted and read the way Python would. The difference is
//! what a wrong guess costs, which the two sections below spell out.
//!
//! # Integers a client may spell as floats
//!
//! ## Why this exists
//!
//! **A real client sends `113.0` where the protocol says `113`**, and the
//! reference server never notices. Python's `json` decodes that to a `float`,
//! and `113.0 == 113` with the same hash, so it indexes the location table, the
//! set membership test, and every comparison downstream exactly as an integer
//! would. Nothing in the reference ever asks what type it was.
//!
//! Rust does ask, and refused: `LocationChecks: invalid type: floating point
//! 113.0, expected i64`. That dropped the connection — matching what the
//! reference does for a genuinely malformed packet — for a packet the reference
//! considers perfectly ordinary. The player saw their client disconnect on
//! every location they checked, and nothing about it was their fault.
//!
//! ## What is and is not accepted
//!
//! A float with no fractional part, inside the target's range. `113.0` is
//! 113; `113.5` and `1e300` are still errors.
//!
//! **Truncating would be worse than refusing.** A fractional location id is not
//! a location this seed has, and silently rounding one into a real id would
//! check somebody's location because a client had a rounding bug. The reference
//! reaches the same outcome by a different route — a `113.5` key simply matches
//! nothing — so refusing the packet and refusing to guess agree with it in
//! every case that matters.
//!
//! This is deliberately confined to *inbound* numbers. What pahoa emits stays
//! strictly typed.
//!
//! # Booleans a client may spell as anything
//!
//! **A real client sends `want_reply: 0`**, and again the reference never
//! notices: it writes `if args.get("want_reply", False):`
//! (`MultiServer.py:2275`) and `if args.get("slot_data", True):` (`:1973`),
//! which are truth tests on whatever `json` produced, not type checks. `0` is
//! falsy in Python, so the packet does exactly what a `false` would have.
//!
//! Rust asked, refused, and dropped the socket: `Set: invalid type: integer 0,
//! expected a boolean`. For a client that stores anything in data storage —
//! most of them, for hints and progress — that is a disconnect on ordinary
//! traffic, seen live.
//!
//! **Here, unlike the integers, being lenient costs nothing.** There is no
//! guess to get wrong: Python's answer is total and defined for every JSON
//! value, so reproducing `bool(x)` is not a tolerance but the actual rule the
//! reference implements. And a wrong reading of either field is cosmetic —
//! whether a `SetReply` is echoed back, whether `slot_data` rides along on
//! `Connected` — where a wrong location id would have checked somebody's
//! location. So these two accept every JSON type and apply Python's own
//! truthiness: `0`, `0.0`, `""`, `[]`, `{}` and `null` are false, everything
//! else is true.

use serde::de::{self, Deserializer, IgnoredAny, Unexpected, Visitor};
use std::fmt;

/// Accepts an integer, or a float that is exactly one.
struct IntVisitor;

impl Visitor<'_> for IntVisitor {
    type Value = i64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an integer, or a float with no fractional part")
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
        Ok(v)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
        i64::try_from(v).map_err(|_| E::invalid_value(Unexpected::Unsigned(v), &self))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
        // `as` would saturate rather than fail, turning 1e300 into i64::MAX and
        // a nonsense location id into a plausible one.
        if v.fract() == 0.0 && v >= -(2f64.powi(63)) && v < 2f64.powi(63) {
            Ok(v as i64)
        } else {
            Err(E::invalid_value(Unexpected::Float(v), &self))
        }
    }
}

fn int<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    d.deserialize_any(IntVisitor)
}

fn narrow<E: de::Error, T: TryFrom<i64>>(v: i64) -> Result<T, E> {
    T::try_from(v).map_err(|_| E::invalid_value(Unexpected::Signed(v), &"a value in range"))
}

pub fn i64_<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    int(d)
}

pub fn u32_<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    narrow(int(d)?)
}

pub fn u8_<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    narrow(int(d)?)
}

/// A sequence of them, which is the case the live failure came from:
/// `LocationChecks.locations`.
pub fn i64_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i64>, D::Error> {
    struct SeqVisitor;

    impl<'de> Visitor<'de> for SeqVisitor {
        type Value = Vec<i64>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a list of integers")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<i64>, A::Error> {
            // Wrapper so each element goes through `IntVisitor` rather than
            // serde's own `i64` impl.
            struct Element(i64);
            impl<'d> serde::Deserialize<'d> for Element {
                fn deserialize<D: Deserializer<'d>>(d: D) -> Result<Self, D::Error> {
                    int(d).map(Element)
                }
            }
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1024));
            while let Some(Element(v)) = seq.next_element()? {
                out.push(v);
            }
            Ok(out)
        }
    }

    d.deserialize_seq(SeqVisitor)
}

/// `Option` of the above. An absent field and an explicit `null` both stay
/// `None`, which is what the packets that use this already mean by it.
pub fn opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    struct OptVisitor;

    impl<'de> Visitor<'de> for OptVisitor {
        type Value = Option<i64>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an optional integer")
        }

        fn visit_none<E: de::Error>(self) -> Result<Option<i64>, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Option<i64>, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Option<i64>, D::Error> {
            int(d).map(Some)
        }
    }

    d.deserialize_option(OptVisitor)
}

pub fn opt_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    match opt_i64(d)? {
        None => Ok(None),
        Some(v) => narrow(v).map(Some),
    }
}

/// The same tolerance as a type rather than a `deserialize_with`, for fields
/// wrapped in [`crate::Arg`] — which needs a `T` that deserializes itself.
macro_rules! lenient_int {
    ($name:ident, $ty:ty, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(pub $ty);

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                narrow(int(d)?).map($name)
            }
        }
    };
}

lenient_int!(I64, i64, "`i64`, accepting the float spelling.");
lenient_int!(U32, u32, "`u32`, accepting the float spelling.");
lenient_int!(U8, u8, "`u8`, accepting the float spelling.");

/// The same rule applied to an already-parsed value, for the id lists the
/// reference leaves raw and inspects element by element.
pub fn as_int(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| {
            let f = n.as_f64()?;
            (f.fract() == 0.0 && f >= -(2f64.powi(63)) && f < 2f64.powi(63)).then_some(f as i64)
        }),
        _ => None,
    }
}

/// `bool(v)` as Python computes it, for any JSON value.
struct TruthVisitor;

impl<'de> Visitor<'de> for TruthVisitor {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a boolean, or any value Python would test for truth")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
        Ok(v)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<bool, E> {
        Ok(v != 0)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<bool, E> {
        Ok(v != 0)
    }

    /// NaN is truthy in Python. JSON cannot carry one, but the arm should not
    /// be the reason it would differ.
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<bool, E> {
        Ok(v != 0.0)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
        Ok(!v.is_empty())
    }

    fn visit_none<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_any(self)
    }

    /// Drained rather than peeked: leaving elements unread would leave the
    /// enclosing packet mid-value, which serde_json reports as a syntax error
    /// on the *next* field.
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let mut any = false;
        while seq.next_element::<IgnoredAny>()?.is_some() {
            any = true;
        }
        Ok(any)
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut any = false;
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
            any = true;
        }
        Ok(any)
    }
}

pub fn bool_<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    d.deserialize_any(TruthVisitor)
}
