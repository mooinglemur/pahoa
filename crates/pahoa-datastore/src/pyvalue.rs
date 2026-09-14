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

use serde_json::Value;
use std::cmp::Ordering;

/// Python's numeric tower, narrowed to what JSON can carry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PyNum {
    Int(i64),
    Float(f64),
}

impl PyNum {
    pub fn as_f64(self) -> f64 {
        match self {
            PyNum::Int(i) => i as f64,
            PyNum::Float(f) => f,
        }
    }

    pub fn to_value(self) -> Option<Value> {
        match self {
            PyNum::Int(i) => Some(Value::from(i)),
            PyNum::Float(f) => py_repr_f64(f)
                .and_then(|s| serde_json::from_str(&s).ok())
                .map(Value::Number),
        }
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
        Value::Bool(b) => Some(PyNum::Int(*b as i64)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(PyNum::Int(i))
            } else {
                n.as_f64().map(PyNum::Float)
            }
        }
        _ => None,
    }
}

/// Promote to a common type, as Python does before an arithmetic operation.
pub fn coerce(a: PyNum, b: PyNum) -> (PyNum, PyNum) {
    match (a, b) {
        (PyNum::Int(_), PyNum::Float(_)) | (PyNum::Float(_), PyNum::Int(_)) => {
            (PyNum::Float(a.as_f64()), PyNum::Float(b.as_f64()))
        }
        _ => (a, b),
    }
}

/// `a % b` with Python's sign convention: the result follows the *divisor*.
pub fn floor_mod_i64(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    // wrapping_rem, because `i64::MIN % -1` overflows in Rust while Python
    // simply answers 0.
    let m = a.wrapping_rem(b);
    Some(if m != 0 && ((m < 0) != (b < 0)) {
        m.wrapping_add(b)
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
    match (as_num(a), as_num(b)) {
        (Some(x), Some(y)) => x.as_f64() == y.as_f64(),
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
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return x.as_f64().partial_cmp(&y.as_f64());
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

    #[test]
    fn booleans_are_integers() {
        assert_eq!(as_num(&json!(true)), Some(PyNum::Int(1)));
        assert_eq!(as_num(&json!(false)), Some(PyNum::Int(0)));
    }

    #[test]
    fn modulo_follows_the_divisors_sign() {
        // Python: -7 % 3 == 2, 7 % -3 == -2. Rust's % gives -1 and 1.
        assert_eq!(floor_mod_i64(-7, 3), Some(2));
        assert_eq!(floor_mod_i64(7, -3), Some(-2));
        assert_eq!(floor_mod_i64(7, 3), Some(1));
        assert_eq!(floor_mod_i64(-7, -3), Some(-1));
        assert_eq!(floor_mod_i64(6, 3), Some(0));
        assert_eq!(floor_mod_i64(1, 0), None);
        // Python answers 0; Rust's `%` would overflow.
        assert_eq!(floor_mod_i64(i64::MIN, -1), Some(0));
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
