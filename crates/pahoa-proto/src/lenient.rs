//! Integers a client may spell as floats.
//!
//! # Why this exists
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
//! # What is and is not accepted
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

use serde::de::{self, Deserializer, Unexpected, Visitor};
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
