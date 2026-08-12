//! The eighteen data-storage operations (`MultiServer.py:109-134`).
//!
//! Each is a Python expression applied to client-supplied JSON, so the work
//! here is reproducing CPython's behavior — including the parts that look like
//! bugs, because clients may depend on them.
//!
//! Four deliberate divergences, all narrower than they sound:
//!
//! 1. **Bounded integers.** Python's ints are arbitrary precision, so
//!    `pow(2, 10**9)` does not error — it hangs and exhausts memory. That is a
//!    remote denial of service in the reference server. Arithmetic here is
//!    checked `i64`; overflow becomes an error, which produces the same
//!    *observable shape* as Python (exception → connection dropped) for every
//!    input a real client sends.
//! 2. **Bounded sequences.** `"x" * 10**9` likewise. Results larger than
//!    [`MAX_RESULT_LEN`] are refused.
//! 3. **No non-finite floats.** Python emits bare `Infinity`/`NaN`, which are
//!    not valid JSON and would corrupt the frame for every recipient.
//! 4. **Transactional.** Python mutates the stored object in place for
//!    `remove`/`pop`/`update`, so a later operation raising leaves the earlier
//!    ones applied (`MultiServer.py:2183-2189`). That is a latent bug no client
//!    can sanely depend on; here a failed sequence changes nothing.

use crate::pyvalue::{self, PyNum};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use thiserror::Error;

/// Cap on strings and arrays produced by an operation. Generous for real use,
/// small enough that `"x" * 10**9` cannot exhaust memory.
pub const MAX_RESULT_LEN: usize = 16 * 1024 * 1024;

/// Failure modes, named after the Python exception they stand in for.
///
/// The room turns any of these into the same outcome the reference server
/// produces: the connection is dropped rather than answered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OpError {
    #[error("unsupported operand type(s) for {op}: {left} and {right}")]
    TypeError {
        op: &'static str,
        left: &'static str,
        right: &'static str,
    },

    #[error("{0}")]
    ValueError(String),

    #[error("list index out of range")]
    IndexError,

    #[error("key not found")]
    KeyError,

    #[error("division or modulo by zero")]
    ZeroDivisionError,

    #[error("unhashable type in container")]
    Unhashable,

    #[error("unknown data storage operation {0:?}")]
    UnknownOperation(String),

    #[error("arithmetic overflow (pahoa bounds integers to 64 bits)")]
    Overflow,

    #[error("result would exceed {MAX_RESULT_LEN} bytes")]
    ResultTooLarge,

    #[error("result is not a finite number")]
    NotFinite,

    /// `%` on a string is printf-style formatting in Python, not modulo:
    /// `"%s" % [1, "a"]` yields `[1, 'a']`, complete with Python's `repr`
    /// quoting. Reproducing that faithfully means reproducing `repr` for
    /// arbitrary values, and a *partial* printf would be worse than none —
    /// it would silently produce wrong strings for untested inputs.
    ///
    /// The protocol documents `mod` as numeric modulo, and no Archipelago
    /// client formats strings through a shared key-value store, so this is
    /// refused outright. Refusing produces the same observable outcome as any
    /// other type error: the connection is dropped.
    #[error("string formatting via `mod` is not supported (see OpError docs)")]
    StringFormatting,
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "None",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

type OpResult = Result<Value, OpError>;

/// What Python gets when it iterates a value.
///
/// Strings yield their characters and dicts yield their keys, which is why
/// `update([], "ab")` produces `["a", "b"]` rather than appending the string.
/// Numbers, booleans and `None` are not iterable at all.
fn iterate(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Array(items) => Some(items.clone()),
        Value::String(s) => Some(s.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::Object(map) => Some(map.keys().map(|k| Value::String(k.clone())).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn num_value(n: PyNum) -> OpResult {
    match n {
        PyNum::Float(f) if !f.is_finite() => Err(OpError::NotFinite),
        other => other.to_value().ok_or(OpError::NotFinite),
    }
}

fn need_nums(op: &'static str, a: &Value, b: &Value) -> Result<(PyNum, PyNum), OpError> {
    match (pyvalue::as_num(a), pyvalue::as_num(b)) {
        (Some(x), Some(y)) => Ok(pyvalue::coerce(x, y)),
        _ => Err(OpError::TypeError {
            op,
            left: type_name(a),
            right: type_name(b),
        }),
    }
}

/// Apply one operation, returning the new value.
///
/// `current` is taken by value and never aliases the stored object, which is
/// what makes a sequence of operations all-or-nothing.
pub fn apply(op: &str, current: Value, arg: &Value) -> OpResult {
    match op {
        "replace" => Ok(arg.clone()),
        // Keeps the existing value; the caller has already substituted the
        // packet's `default` if the key was absent.
        "default" => Ok(current),

        "add" => add(current, arg),
        "mul" => mul(current, arg),
        "pow" => pow(&current, arg),
        "mod" => modulo(&current, arg),
        "floor" => round(&current, f64::floor),
        "ceil" => round(&current, f64::ceil),
        "max" => pick("max", current, arg, Ordering::Less),
        "min" => pick("min", current, arg, Ordering::Greater),

        "xor" => bitwise("^", &current, arg, |a, b| a ^ b),
        "or" => or(current, arg),
        "and" => bitwise("&", &current, arg, |a, b| a & b),
        "left_shift" => shift(&current, arg, true),
        "right_shift" => shift(&current, arg, false),

        "remove" => remove(current, arg),
        "pop" => pop(current, arg),
        "update" => update(current, arg),

        other => Err(OpError::UnknownOperation(other.to_string())),
    }
}

/// `+`: numeric addition, string concatenation, or list extension.
fn add(current: Value, arg: &Value) -> OpResult {
    match (&current, arg) {
        (Value::String(a), Value::String(b)) => {
            let len = a.len() + b.len();
            if len > MAX_RESULT_LEN {
                return Err(OpError::ResultTooLarge);
            }
            Ok(Value::String(format!("{a}{b}")))
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() + b.len() > MAX_RESULT_LEN {
                return Err(OpError::ResultTooLarge);
            }
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            Ok(Value::Array(out))
        }
        _ => {
            let (x, y) = need_nums("+", &current, arg)?;
            match (x, y) {
                (PyNum::Int(a), PyNum::Int(b)) => {
                    num_value(PyNum::Int(a.checked_add(b).ok_or(OpError::Overflow)?))
                }
                _ => num_value(PyNum::Float(x.as_f64() + y.as_f64())),
            }
        }
    }
}

/// `*`: numeric multiplication, or sequence repetition.
fn mul(current: Value, arg: &Value) -> OpResult {
    // Python allows either order: `3 * "ab"` and `"ab" * 3` both work.
    let repeat = |seq: &Value, count: &Value| -> Option<OpResult> {
        let n = match pyvalue::as_num(count) {
            Some(PyNum::Int(n)) => n,
            _ => return None,
        };
        let n = n.max(0) as usize;
        Some(match seq {
            Value::String(s) => {
                if s.len().saturating_mul(n) > MAX_RESULT_LEN {
                    Err(OpError::ResultTooLarge)
                } else {
                    Ok(Value::String(s.repeat(n)))
                }
            }
            Value::Array(a) => {
                if a.len().saturating_mul(n) > MAX_RESULT_LEN {
                    Err(OpError::ResultTooLarge)
                } else {
                    let mut out = Vec::with_capacity(a.len() * n);
                    for _ in 0..n {
                        out.extend(a.iter().cloned());
                    }
                    Ok(Value::Array(out))
                }
            }
            _ => return None,
        })
    };

    if matches!(current, Value::String(_) | Value::Array(_))
        && let Some(r) = repeat(&current, arg)
    {
        return r;
    }
    if matches!(arg, Value::String(_) | Value::Array(_))
        && let Some(r) = repeat(arg, &current)
    {
        return r;
    }

    let (x, y) = need_nums("*", &current, arg)?;
    match (x, y) {
        (PyNum::Int(a), PyNum::Int(b)) => {
            num_value(PyNum::Int(a.checked_mul(b).ok_or(OpError::Overflow)?))
        }
        _ => num_value(PyNum::Float(x.as_f64() * y.as_f64())),
    }
}

fn pow(current: &Value, arg: &Value) -> OpResult {
    let (x, y) = need_nums("**", current, arg)?;
    match (x, y) {
        (PyNum::Int(a), PyNum::Int(b)) if b >= 0 => {
            let exp = u32::try_from(b).map_err(|_| OpError::Overflow)?;
            num_value(PyNum::Int(a.checked_pow(exp).ok_or(OpError::Overflow)?))
        }
        // A negative integer exponent produces a float in Python 3.
        _ => num_value(PyNum::Float(x.as_f64().powf(y.as_f64()))),
    }
}

fn modulo(current: &Value, arg: &Value) -> OpResult {
    if matches!(current, Value::String(_)) {
        return Err(OpError::StringFormatting);
    }
    let (x, y) = need_nums("%", current, arg)?;
    match (x, y) {
        (PyNum::Int(a), PyNum::Int(b)) => num_value(PyNum::Int(
            pyvalue::floor_mod_i64(a, b).ok_or(OpError::ZeroDivisionError)?,
        )),
        _ => num_value(PyNum::Float(
            pyvalue::floor_mod_f64(x.as_f64(), y.as_f64()).ok_or(OpError::ZeroDivisionError)?,
        )),
    }
}

/// `floor`/`ceil` ignore the argument entirely and return an int.
fn round(current: &Value, f: fn(f64) -> f64) -> OpResult {
    match pyvalue::as_num(current) {
        Some(PyNum::Int(i)) => Ok(Value::from(i)),
        Some(PyNum::Float(x)) => {
            let r = f(x);
            if !r.is_finite() {
                return Err(OpError::NotFinite);
            }
            if r < i64::MIN as f64 || r > i64::MAX as f64 {
                return Err(OpError::Overflow);
            }
            Ok(Value::from(r as i64))
        }
        None => Err(OpError::TypeError {
            op: "floor/ceil",
            left: type_name(current),
            right: "None",
        }),
    }
}

/// `max`/`min`.
///
/// Two behaviors worth stating, both caught by the CPython vectors:
///
/// - Python returns the **first** maximal element, so `max(1, 1.0)` is the int
///   `1` while `max(1.0, 1)` is the float — visible in the emitted JSON.
/// - Operands Python cannot order (`max(1, "a")`, anything with `None`) raise
///   `TypeError`. Quietly keeping the current value instead would leave the
///   client believing its write landed.
///
/// `worse` is the ordering that means "take the argument instead".
fn pick(op: &'static str, current: Value, arg: &Value, worse: Ordering) -> OpResult {
    match pyvalue::py_cmp(&current, arg) {
        Some(o) if o == worse => Ok(arg.clone()),
        // Equal keeps `current`, which is what "first maximal" means.
        Some(_) => Ok(current),
        None => Err(OpError::TypeError {
            op,
            left: type_name(&current),
            right: type_name(arg),
        }),
    }
}

fn bitwise(op: &'static str, current: &Value, arg: &Value, f: fn(i64, i64) -> i64) -> OpResult {
    // `True & True` is `True` in Python, not `1`.
    if let (Value::Bool(a), Value::Bool(b)) = (current, arg) {
        return Ok(Value::Bool(f(*a as i64, *b as i64) != 0));
    }
    match (pyvalue::as_num(current), pyvalue::as_num(arg)) {
        (Some(PyNum::Int(a)), Some(PyNum::Int(b))) => Ok(Value::from(f(a, b))),
        _ => Err(OpError::TypeError {
            op,
            left: type_name(current),
            right: type_name(arg),
        }),
    }
}

/// `|`: bitwise or on integers, dict merge on dicts (Python 3.9+).
fn or(current: Value, arg: &Value) -> OpResult {
    if let (Value::Object(a), Value::Object(b)) = (&current, arg) {
        let mut merged: Map<String, Value> = a.clone();
        for (k, v) in b {
            merged.insert(k.clone(), v.clone());
        }
        return Ok(Value::Object(merged));
    }
    bitwise("|", &current, arg, |a, b| a | b)
}

fn shift(current: &Value, arg: &Value, left: bool) -> OpResult {
    let op = if left { "<<" } else { ">>" };
    match (pyvalue::as_num(current), pyvalue::as_num(arg)) {
        (Some(PyNum::Int(a)), Some(PyNum::Int(b))) => {
            if b < 0 {
                return Err(OpError::ValueError("negative shift count".into()));
            }
            let bits = u32::try_from(b).map_err(|_| OpError::Overflow)?;
            let result = if left {
                a.checked_shl(bits)
                    .filter(|r| r >> bits == a)
                    .ok_or(OpError::Overflow)?
            } else {
                // Python's >> on a negative int is an arithmetic shift, and
                // shifting past the width saturates toward the sign.
                if bits >= 64 {
                    if a < 0 { -1 } else { 0 }
                } else {
                    a >> bits
                }
            };
            Ok(Value::from(result))
        }
        _ => Err(OpError::TypeError {
            op,
            left: type_name(current),
            right: type_name(arg),
        }),
    }
}

/// `list.remove(value)`: drops the **first** match; absent is a silent no-op.
///
/// Python catches `ValueError` here but nothing else, so calling this on a
/// non-list raises `AttributeError` and drops the connection
/// (`MultiServer.py:64-70`).
fn remove(current: Value, arg: &Value) -> OpResult {
    match current {
        Value::Array(mut items) => {
            if let Some(i) = items.iter().position(|v| pyvalue::py_eq(v, arg)) {
                items.remove(i);
            }
            Ok(Value::Array(items))
        }
        other => Err(OpError::TypeError {
            op: "remove",
            left: type_name(&other),
            right: type_name(arg),
        }),
    }
}

/// `container.pop(value)`, with Python's asymmetric guards
/// (`MultiServer.py:72-83`).
///
/// A list index at or beyond the length is guarded and becomes a no-op, and a
/// missing dict key likewise — but a **negative** out-of-range index is not
/// guarded, and the resulting `IndexError` is not among the exceptions Python
/// catches, so it propagates and drops the connection. That asymmetry is real
/// behavior, so it is reproduced rather than tidied.
fn pop(current: Value, arg: &Value) -> OpResult {
    match current {
        Value::Array(mut items) => {
            let Some(PyNum::Int(i)) = pyvalue::as_num(arg) else {
                return Err(OpError::TypeError {
                    op: "pop",
                    left: "list",
                    right: type_name(arg),
                });
            };
            // Guarded: non-negative and out of range does nothing.
            if i >= 0 && (i as usize) >= items.len() {
                return Ok(Value::Array(items));
            }
            let index = if i < 0 {
                let from_end = items.len() as i64 + i;
                if from_end < 0 {
                    // Unguarded in Python: raises IndexError.
                    return Err(OpError::IndexError);
                }
                from_end as usize
            } else {
                i as usize
            };
            items.remove(index);
            Ok(Value::Array(items))
        }
        Value::Object(mut map) => {
            match arg {
                // JSON object keys are strings, so only a string can match one.
                Value::String(s) => {
                    // Guarded: a missing key is a no-op.
                    map.shift_remove(s);
                }
                // Hashable but never equal to a string key, so Python's
                // `value not in container` guard makes this a no-op too.
                // Stringifying here would wrongly let `pop({"1": …}, 1)` hit.
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
                // Unhashable: `dict.pop([])` raises TypeError before any lookup.
                Value::Array(_) | Value::Object(_) => {
                    return Err(OpError::TypeError {
                        op: "pop",
                        left: "dict",
                        right: type_name(arg),
                    });
                }
            }
            Ok(Value::Object(map))
        }
        other => Err(OpError::TypeError {
            op: "pop",
            left: type_name(&other),
            right: type_name(arg),
        }),
    }
}

/// `update`: append-if-absent for lists, `dict.update` for dicts
/// (`MultiServer.py:85-92`).
///
/// The list branch builds `set(container)` first, so an unhashable element —
/// a nested list or dict — raises `TypeError` and drops the connection. And
/// membership uses Python equality, so `[1]` updated with `[1.0]` appends
/// nothing.
fn update(current: Value, arg: &Value) -> OpResult {
    match (current, arg) {
        (Value::Array(mut items), entries) => {
            // `entries` is any Python iterable, not just a list: a string
            // yields its characters and a dict yields its keys.
            let entries = iterate(entries).ok_or(OpError::TypeError {
                op: "update",
                left: "list",
                right: type_name(entries),
            })?;

            // `set(container)` hashes every existing element...
            if !items.iter().all(pyvalue::is_hashable) {
                return Err(OpError::Unhashable);
            }
            // ...and is computed **once**, before anything is appended
            // (`MultiServer.py:86-88`). Entries are therefore filtered against
            // the original contents, not against the growing list — so
            // duplicates *within* the entries are all appended. Testing against
            // `items` as it grows would silently collapse them.
            let original = items.clone();
            for entry in entries {
                // `entry not in <set>` hashes each candidate too, so an
                // unhashable entry raises even when the container is empty.
                if !pyvalue::is_hashable(&entry) {
                    return Err(OpError::Unhashable);
                }
                if !pyvalue::py_contains(&original, &entry) {
                    if items.len() >= MAX_RESULT_LEN {
                        return Err(OpError::ResultTooLarge);
                    }
                    items.push(entry);
                }
            }
            Ok(Value::Array(items))
        }
        (Value::Object(mut map), Value::Object(entries)) => {
            for (k, v) in entries {
                map.insert(k.clone(), v.clone());
            }
            Ok(Value::Object(map))
        }
        // `dict.update` also accepts any iterable of pairs — including a
        // string, which yields characters, each of which is then a 1-element
        // sequence and so a length error. An *empty* string yields nothing and
        // is a legitimate no-op.
        (Value::Object(mut map), other) => {
            let elements = iterate(other).ok_or(OpError::TypeError {
                op: "update",
                left: "dict",
                right: type_name(other),
            })?;
            for (i, element) in elements.iter().enumerate() {
                // Each element must itself be a 2-element sequence.
                let kv = iterate(element).ok_or(OpError::TypeError {
                    op: "update",
                    left: "dict",
                    right: type_name(element),
                })?;
                if kv.len() != 2 {
                    return Err(OpError::ValueError(format!(
                        "dictionary update sequence element #{i} has length {}; 2 is required",
                        kv.len()
                    )));
                }
                match &kv[0] {
                    Value::String(s) => {
                        map.insert(s.clone(), kv[1].clone());
                    }
                    // Python would accept any hashable key, but a JSON object
                    // cannot hold one; coercing would invent a key the client
                    // never asked for.
                    _ => {
                        return Err(OpError::TypeError {
                            op: "update",
                            left: "dict key",
                            right: type_name(&kv[0]),
                        });
                    }
                }
            }
            Ok(Value::Object(map))
        }
        (current, arg) => Err(OpError::TypeError {
            op: "update",
            left: type_name(&current),
            right: type_name(arg),
        }),
    }
}

/// Apply a whole `Set` sequence.
///
/// All-or-nothing: the value is only stored if every operation succeeds. See
/// the module docs for why that differs from the reference.
pub fn apply_all(
    mut value: Value,
    operations: &[(String, Value)],
) -> Result<Value, (usize, OpError)> {
    for (index, (op, arg)) in operations.iter().enumerate() {
        value = apply(op, value, arg).map_err(|e| (index, e))?;
    }
    Ok(value)
}
