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
            PyNum::Float(f) => serde_json::Number::from_f64(f).map(Value::Number),
        }
    }
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
    Some(if m != 0.0 && ((m < 0.0) != (b < 0.0)) {
        m + b
    } else {
        m
    })
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
