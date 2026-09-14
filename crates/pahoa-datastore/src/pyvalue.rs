//! Python value semantics over JSON.
//!
//! Data storage operations are Python expressions applied to client-supplied
//! values, so reproducing them means reproducing a slice of CPython's numeric
//! and sequence protocol rather than writing eighteen ad-hoc match arms.
//!
//! The traps this module exists to encode:
//!
//! - **`bool` is a subclass of `int`.** `True + 1 == 2`. JSON booleans have to
//!   participate in arithmetic and bitwise operations as 1 and 0.
//! - **`%` is floored, not truncated.** Python's `-7 % 3 == 2`; Rust's is `-1`.
//! - **Equality crosses types.** `1 == 1.0 == True`, which decides what
//!   `update` considers a duplicate and what `remove` removes.

use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};
use serde_json::Value;
use std::cmp::Ordering;

/// Python's numeric tower, narrowed to what JSON can carry.
///
/// `Int` is arbitrary precision because Python's is, and because a client
/// depended on it: a world packing its location checks into a 71-bit bitfield
/// had every `or` that set a bit refused, back when this held an `i64`. What
/// is *not* unbounded is how wide a value the operations will build — see
/// [`crate::ops::MAX_INT_BITS`].
#[derive(Debug, Clone, PartialEq)]
pub enum PyNum {
    Int(BigInt),
    Float(f64),
}

impl PyNum {
    /// The value as an `f64`, as Python's `float()` would produce it.
    ///
    /// `None` exactly where Python raises `OverflowError`: an integer too large
    /// for a double has no float value, and answering `inf` would quietly make
    /// every such integer compare equal to every other one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            PyNum::Int(i) => i.to_f64().filter(|f| f.is_finite()),
            PyNum::Float(f) => Some(*f),
        }
    }

    pub fn to_value(&self) -> Option<Value> {
        match self {
            // The common case stays off the string path entirely.
            PyNum::Int(i) => match i.to_i64() {
                Some(small) => Some(Value::from(small)),
                // `arbitrary_precision` is what lets this survive: the digits
                // are kept as written rather than folded into a double.
                None => serde_json::from_str(&i.to_string()).ok(),
            },
            PyNum::Float(f) => py_repr_f64(*f)
                .and_then(|s| serde_json::from_str(&s).ok())
                .map(Value::Number),
        }
    }
}

/// Compare an exact integer with a float, the way Python does.
///
/// Python does **not** convert the int to a double first — that would make
/// `2**71 + 1 == 2.3611832414348226e+21` true, because the conversion rounds
/// onto exactly that double. It compares against the float's integer part and
/// lets the fraction break the tie, which is what this reproduces.
fn cmp_int_f64(a: &BigInt, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if f == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    let truncated = f.trunc();
    // Every finite double with no fractional part is an integer exactly, so
    // this conversion is lossless in the direction that matters.
    let whole = BigInt::from_f64(truncated)?;
    Some(match a.cmp(&whole) {
        Ordering::Equal => {
            // Equal integer parts, so the fraction decides — and it carries the
            // float's own sign, which is what makes `-2 > -2.5`.
            let fraction = f - truncated;
            if fraction > 0.0 {
                Ordering::Less
            } else if fraction < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    })
}

/// Order two numbers as Python orders them, across the int/float divide.
pub fn cmp_nums(x: &PyNum, y: &PyNum) -> Option<Ordering> {
    match (x, y) {
        (PyNum::Int(a), PyNum::Int(b)) => Some(a.cmp(b)),
        (PyNum::Float(a), PyNum::Float(b)) => a.partial_cmp(b),
        (PyNum::Int(a), PyNum::Float(f)) => cmp_int_f64(a, *f),
        (PyNum::Float(f), PyNum::Int(b)) => cmp_int_f64(b, *f).map(Ordering::reverse),
    }
}

/// Both operands as `f64`, as Python's mixed int/float arithmetic does it.
///
/// `None` where Python raises `OverflowError` converting the int.
pub fn as_floats(x: &PyNum, y: &PyNum) -> Option<(f64, f64)> {
    Some((x.as_f64()?, y.as_f64()?))
}

/// The two numbers a `Value` pair holds, if both are numbers and both are small
/// enough to answer without allocating.
///
/// `py_eq` runs once per element of a list for `update` and `remove`, so the
/// overwhelmingly common comparison — two ordinary integers — should not build
/// two heap-allocated bignums to reach its answer.
fn small_ints(a: &Value, b: &Value) -> Option<(i64, i64)> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => Some((x.as_i64()?, y.as_i64()?)),
        _ => None,
    }
}

/// Render a finite `f64` the way CPython's `repr` does.
///
/// The *digits* already agree — serde_json and CPython both emit the shortest
/// string that round-trips — but the layout differs in two places, and with
/// `arbitrary_precision` those differences survive onto the wire instead of
/// being flattened back into an `f64` by the next thing to touch them:
///
/// - CPython goes exponential below `1e-4`, where serde_json keeps writing
///   zeros: `1e-05` against `0.00001`.
/// - CPython pads an exponent to two digits: `1e-06` against `1e-6`. Only
///   single-digit exponents differ — `1e+100` and `5e-324` already agree, as
///   does every positive exponent, since serde_json writes the `+` too.
///
/// Returns `None` for a non-finite float, which has no JSON spelling at all.
pub fn py_repr_f64(f: f64) -> Option<String> {
    if !f.is_finite() {
        return None;
    }
    // `f != 0.0` is false for `-0.0` as well as `0.0`, which is what we want:
    // both render as CPython renders them, and neither wants exponent form.
    let s = if f != 0.0 && f.abs() < 1e-4 {
        // Rust's `LowerExp` is shortest round-trip too, so this is the same
        // digits laid out the other way rather than a reformatting.
        format!("{f:e}")
    } else {
        serde_json::Number::from_f64(f)?.to_string()
    };

    let Some((mantissa, exponent)) = s.split_once('e') else {
        return Some(s);
    };
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("+", exponent.trim_start_matches('+')),
    };
    Some(format!("{mantissa}e{sign}{digits:0>2}"))
}

/// Interpret a JSON value as a number, treating booleans as ints.
pub fn as_num(v: &Value) -> Option<PyNum> {
    match v {
        // `True` is `1` in every arithmetic and bitwise context.
        Value::Bool(b) => Some(PyNum::Int(BigInt::from(*b as u8))),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(PyNum::Int(BigInt::from(i)));
            }
            // Wider than an `i64` and still an integer: `arbitrary_precision`
            // kept the digits, so parse them rather than accepting the double.
            // A float's text (`1.5`, `1e5`) fails this and falls through, which
            // is the sorting Python's own parser does.
            if let Ok(big) = n.to_string().parse::<BigInt>() {
                return Some(PyNum::Int(big));
            }
            n.as_f64().map(PyNum::Float)
        }
        _ => None,
    }
}

/// `a % b` with Python's sign convention: the result follows the *divisor*.
pub fn floor_mod_big(a: &BigInt, b: &BigInt) -> Option<BigInt> {
    if b.is_zero() {
        return None;
    }
    let m = a % b;
    Some(if !m.is_zero() && (m.is_negative() != b.is_negative()) {
        m + b
    } else {
        m
    })
}

pub fn floor_mod_f64(a: f64, b: f64) -> Option<f64> {
    if b == 0.0 {
        return None;
    }
    let m = a % b;
    // A zero remainder takes the sign of the **divisor**, not whatever `fmod`
    // happened to return — CPython does this explicitly because platforms
    // disagree about signed zero here (`Objects/floatobject.c`, `float_rem`).
    // So `0 % -2.5` is `-0.0`, and that is a different four bytes on the wire
    // from `0.0` now that numbers keep the text they were written with.
    if m == 0.0 {
        return Some(0.0f64.copysign(b));
    }
    Some(if (m < 0.0) != (b < 0.0) { m + b } else { m })
}

/// Python's `==` across JSON types.
///
/// Numbers compare by value regardless of int/float/bool, which is what makes
/// `1 in [True]` true and stops `update` appending a duplicate.
pub fn py_eq(a: &Value, b: &Value) -> bool {
    if let Some((x, y)) = small_ints(a, b) {
        return x == y;
    }
    match (as_num(a), as_num(b)) {
        (Some(x), Some(y)) => cmp_nums(&x, &y) == Some(Ordering::Equal),
        (None, None) => match (a, b) {
            (Value::String(x), Value::String(y)) => x == y,
            (Value::Null, Value::Null) => true,
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
            }
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
            }
            _ => false,
        },
        // A number never equals a non-number.
        _ => false,
    }
}

/// Python's `<` where it is defined, for `max`/`min`.
///
/// Returns `None` for comparisons Python would refuse, such as `1 < "a"`.
pub fn py_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    if let Some((x, y)) = small_ints(a, b) {
        return Some(x.cmp(&y));
    }
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return cmp_nums(&x, &y);
    }
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Array(x), Value::Array(y)) => {
            // Lexicographic, element by element.
            for (p, q) in x.iter().zip(y) {
                match py_cmp(p, q)? {
                    Ordering::Equal => continue,
                    other => return Some(other),
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        _ => None,
    }
}

/// Is `needle` present in `haystack` under Python equality?
pub fn py_contains(haystack: &[Value], needle: &Value) -> bool {
    haystack.iter().any(|v| py_eq(v, needle))
}

/// Whether a value could be a Python set member.
///
/// `update` on a list builds `set(container)` first, which raises `TypeError`
/// for unhashable elements — lists and dicts (`MultiServer.py:85-92`).
pub fn is_hashable(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Object(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every right-hand side is `repr(v)` from CPython 3, pasted verbatim.
    ///
    /// The interesting rows are the ones serde_json alone gets differently:
    /// `1e-05` (it writes `0.00001`) and the single-digit exponents (it writes
    /// `1e-6`). The rest are here to pin that the fix did not disturb them.
    #[test]
    fn floats_render_as_cpython_reprs_them() {
        for (value, want) in [
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1e-5, "1e-05"),
            (1e-4, "0.0001"),
            (1e100, "1e+100"),
            (1e-100, "1e-100"),
            (1.2142656789020123e-6, "1.2142656789020123e-06"),
            (0.1, "0.1"),
            (1.0, "1.0"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (2.5, "2.5"),
            (1e17, "1e+17"),
            (1.5e-7, "1.5e-07"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (-9.9e-5, "-9.9e-05"),
            (0.00012, "0.00012"),
            (-1e-6, "-1e-06"),
            (3.0, "3.0"),
        ] {
            assert_eq!(py_repr_f64(value).as_deref(), Some(want));
        }
    }

    #[test]
    fn a_rendered_float_still_reads_back_as_itself() {
        // The layout changed; the value must not have. A shortest-round-trip
        // string that no longer round-trips would be a silent corruption of
        // every computed float, which is the failure this whole change exists
        // to stop happening to integers.
        for value in [1e-5, 1.2142656789020123e-6, 5e-324, -9.9e-5, 0.1, 2.5] {
            let text = py_repr_f64(value).unwrap();
            assert_eq!(text.parse::<f64>().unwrap(), value, "{text}");
        }
    }

    #[test]
    fn non_finite_floats_have_no_rendering() {
        assert_eq!(py_repr_f64(f64::NAN), None);
        assert_eq!(py_repr_f64(f64::INFINITY), None);
        assert_eq!(py_repr_f64(f64::NEG_INFINITY), None);
    }

    #[test]
    fn a_zero_remainder_takes_the_divisors_sign() {
        // CPython's own special case, and invisible until numbers kept their
        // text: `0.0` and `-0.0` compare equal as `f64`.
        assert_eq!(
            floor_mod_f64(0.0, -2.5).map(f64::is_sign_negative),
            Some(true)
        );
        assert_eq!(
            floor_mod_f64(2.5, -2.5).map(f64::is_sign_negative),
            Some(true)
        );
        assert_eq!(
            floor_mod_f64(-2.5, 2.5).map(f64::is_sign_negative),
            Some(false)
        );
        assert_eq!(
            floor_mod_f64(2.0, -1.0).map(f64::is_sign_negative),
            Some(true)
        );
        // A nonzero remainder is untouched by the rule.
        assert_eq!(floor_mod_f64(-7.0, 3.0), Some(2.0));
    }

    /// `BigInt::from` for the small literals these tests are written with.
    fn int(i: i64) -> BigInt {
        BigInt::from(i)
    }

    #[test]
    fn booleans_are_integers() {
        assert_eq!(as_num(&json!(true)), Some(PyNum::Int(int(1))));
        assert_eq!(as_num(&json!(false)), Some(PyNum::Int(int(0))));
    }

    #[test]
    fn modulo_follows_the_divisors_sign() {
        // Python: -7 % 3 == 2, 7 % -3 == -2. Rust's % gives -1 and 1.
        let m = |a: i64, b: i64| floor_mod_big(&int(a), &int(b));
        assert_eq!(m(-7, 3), Some(int(2)));
        assert_eq!(m(7, -3), Some(int(-2)));
        assert_eq!(m(7, 3), Some(int(1)));
        assert_eq!(m(-7, -3), Some(int(-1)));
        assert_eq!(m(6, 3), Some(int(0)));
        assert_eq!(m(1, 0), None);
        // The case that needed `wrapping_rem` when this was an `i64`: nothing
        // overflows any more, and Python's answer was 0 all along.
        assert_eq!(m(i64::MIN, -1), Some(int(0)));
    }

    #[test]
    fn an_integer_wider_than_i64_is_read_as_an_integer() {
        // The reported case. Before `arbitrary_precision` this arrived as a
        // float and every bitwise operation on it was a `TypeError`.
        let wide: BigInt = "2361183241434822606849".parse().unwrap();
        let v: Value = serde_json::from_str("2361183241434822606849").unwrap();
        assert_eq!(as_num(&v), Some(PyNum::Int(wide)));
    }

    #[test]
    fn a_float_literal_is_never_mistaken_for_a_wide_integer() {
        // `as_num` reaches for the digit text before settling for a double, so
        // the sorting between the two has to hold for values that only *look*
        // integral.
        for (text, want) in [("1.5", 1.5), ("1e5", 100000.0), ("2.0", 2.0)] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(as_num(&v), Some(PyNum::Float(want)), "{text}");
        }
    }

    #[test]
    fn an_exact_integer_does_not_compare_equal_to_the_double_it_rounds_to() {
        // Python compares int against float exactly rather than converting, so
        // `2**71 + 1` is *not* the double it would round onto. Converting first
        // — which is what this module used to do — makes them equal, and then
        // `remove` drops the wrong element.
        let exact: BigInt = "2361183241434822606849".parse().unwrap();
        let rounded = 2361183241434822606849f64;
        assert_eq!(
            cmp_nums(&PyNum::Int(exact.clone()), &PyNum::Float(rounded)),
            Some(Ordering::Greater)
        );
        // And the double it *is* equal to still compares equal.
        let on_the_nose: BigInt = "2361183241434822606848".parse().unwrap();
        assert_eq!(
            cmp_nums(&PyNum::Int(on_the_nose), &PyNum::Float(rounded)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            cmp_nums(&PyNum::Float(rounded), &PyNum::Int(exact)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn a_fraction_breaks_the_tie_with_its_own_sign() {
        // `-2 > -2.5` and `2 < 2.5`, both decided after the integer parts match.
        let cases = [
            (2, 2.5, Ordering::Less),
            (3, 2.5, Ordering::Greater),
            (-2, -2.5, Ordering::Greater),
            (-3, -2.5, Ordering::Less),
            (2, 2.0, Ordering::Equal),
        ];
        for (a, f, want) in cases {
            assert_eq!(
                cmp_nums(&PyNum::Int(int(a)), &PyNum::Float(f)),
                Some(want),
                "{a} vs {f}"
            );
        }
        assert_eq!(cmp_nums(&PyNum::Int(int(0)), &PyNum::Float(f64::NAN)), None);
        assert_eq!(
            cmp_nums(&PyNum::Int(int(0)), &PyNum::Float(f64::INFINITY)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn float_modulo_matches_python_too() {
        assert_eq!(floor_mod_f64(-7.0, 3.0), Some(2.0));
        assert_eq!(floor_mod_f64(7.5, -3.0), Some(-1.5));
    }

    #[test]
    fn equality_crosses_numeric_types() {
        assert!(py_eq(&json!(1), &json!(1.0)));
        assert!(py_eq(&json!(1), &json!(true)));
        assert!(py_eq(&json!(0), &json!(false)));
        assert!(!py_eq(&json!(1), &json!("1")));
        assert!(!py_eq(&json!(null), &json!(0)));
    }

    #[test]
    fn equality_recurses_through_containers() {
        assert!(py_eq(&json!([1, 2]), &json!([1.0, 2.0])));
        assert!(py_eq(&json!([1, [2]]), &json!([true, [2.0]])));
        assert!(!py_eq(&json!([1, 2]), &json!([1, 2, 3])));
        assert!(py_eq(&json!({"a": 1}), &json!({"a": 1.0})));
        assert!(!py_eq(&json!({"a": 1}), &json!({"b": 1})));
    }

    #[test]
    fn membership_uses_python_equality() {
        // `1.0 in {1}` is True, so update must not append a "duplicate".
        assert!(py_contains(&[json!(1)], &json!(1.0)));
        assert!(py_contains(&[json!(true)], &json!(1)));
        assert!(!py_contains(&[json!("1")], &json!(1)));
    }

    #[test]
    fn ordering_is_defined_within_types_and_absent_across_them() {
        assert_eq!(py_cmp(&json!(1), &json!(2)), Some(Ordering::Less));
        assert_eq!(py_cmp(&json!("a"), &json!("b")), Some(Ordering::Less));
        assert_eq!(py_cmp(&json!([1, 2]), &json!([1, 3])), Some(Ordering::Less));
        // Python raises TypeError comparing an int with a string.
        assert_eq!(py_cmp(&json!(1), &json!("a")), None);
    }

    #[test]
    fn containers_are_unhashable() {
        assert!(is_hashable(&json!(1)));
        assert!(is_hashable(&json!("a")));
        assert!(is_hashable(&json!(null)));
        assert!(!is_hashable(&json!([1])));
        assert!(!is_hashable(&json!({"a": 1})));
    }
}
