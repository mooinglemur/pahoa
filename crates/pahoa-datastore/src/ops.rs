//! The eighteen data-storage operations (`MultiServer.py:109-134`).
//!
//! Each is a Python expression applied to client-supplied JSON, so the work
//! here is reproducing CPython's behavior — including the parts that look like
//! bugs, because clients may depend on them.
//!
//! Four deliberate divergences, all narrower than they sound:
//!
//! 1. **Bounded integer width.** Integers are arbitrary precision here as they
//!    are in Python — a world storing its location checks as a 71-bit bitfield
//!    reached a live room, and back when this was an `i64` every `or` that set
//!    a bit cost that player their connection. What is bounded is how *wide* a
//!    value these operations will build: see [`MAX_INT_BITS`]. Python has no
//!    such bound, which is why `pow(2, 10**9)` is a remote denial of service in
//!    the reference server rather than an error.
//! 2. **Bounded sequences.** `"x" * 10**9` likewise. Results larger than
//!    [`MAX_RESULT_LEN`] are refused.
//! 3. **No non-finite floats.** Python emits bare `Infinity`/`NaN`, which are
//!    not valid JSON and would corrupt the frame for every recipient.
//! 4. **Transactional.** Python mutates the stored object in place for
//!    `remove`/`pop`/`update`, so a later operation raising leaves the earlier
//!    ones applied (`MultiServer.py:2183-2189`). That is a latent bug no client
//!    can sanely depend on; here a failed sequence changes nothing.

use crate::pyvalue::{self, PyNum};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use thiserror::Error;

/// Cap on strings and arrays produced by an operation. Generous for real use,
/// small enough that `"x" * 10**9` cannot exhaust memory.
pub const MAX_RESULT_LEN: usize = 16 * 1024 * 1024;

/// Cap on the width of an integer these operations will accept or produce.
///
/// 65,536 bits is 8 KiB, a little under 19,729 decimal digits, and 65,536
/// independent bit flags — an order of magnitude beyond the largest Archipelago
/// world's location count, and the reported case that motivated arbitrary
/// precision at all used 71 of them.
///
/// **The bound is about time, not memory.** These operations run on the actor
/// task, the one thread that owns all room state and must never stall; a room
/// that pauses for every client while somebody's tracker multiplies two
/// million-bit numbers is a worse failure than a refused `Set`. At this width
/// every operation here is microseconds. The reference server has no bound at
/// all, which is why `pow(2, 10**9)` is a remote memory-exhaustion path in it.
///
/// Raising it is a one-line change if a real world ever needs more; nothing
/// depends on the specific number.
pub const MAX_INT_BITS: u64 = 65_536;

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

    /// Python's own `OverflowError`: an integer too large to become a float,
    /// which is what mixed int/float arithmetic needs. Not a divergence — the
    /// reference raises here too.
    #[error("integer too large to convert to a float")]
    Overflow,

    /// pahoa's width bound, which the reference does not have. See
    /// [`MAX_INT_BITS`].
    #[error("integer wider than {MAX_INT_BITS} bits")]
    IntegerTooWide,

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

/// Hand back an integer result, refusing one wider than [`MAX_INT_BITS`].
fn int_value(v: BigInt) -> OpResult {
    if v.bits() > MAX_INT_BITS {
        return Err(OpError::IntegerTooWide);
    }
    PyNum::Int(v).to_value().ok_or(OpError::NotFinite)
}

/// A float result from two operands Python would have converted.
fn float_value(x: &PyNum, y: &PyNum, f: impl FnOnce(f64, f64) -> f64) -> OpResult {
    let (a, b) = pyvalue::as_floats(x, y).ok_or(OpError::Overflow)?;
    num_value(PyNum::Float(f(a, b)))
}

fn need_nums(op: &'static str, a: &Value, b: &Value) -> Result<(PyNum, PyNum), OpError> {
    match (pyvalue::as_num(a), pyvalue::as_num(b)) {
        (Some(x), Some(y)) => {
            // Checked on the way in as well as the way out. A value wider than
            // the bound can still be *stored* and read back — storage is
            // verbatim passthrough, and costs nothing — but it is not something
            // the actor will do arithmetic on.
            for n in [&x, &y] {
                if let PyNum::Int(i) = n
                    && i.bits() > MAX_INT_BITS
                {
                    return Err(OpError::IntegerTooWide);
                }
            }
            Ok((x, y))
        }
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
            match (&x, &y) {
                (PyNum::Int(a), PyNum::Int(b)) => int_value(a + b),
                _ => float_value(&x, &y, |a, b| a + b),
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
        // CPython converts the count to a `Py_ssize_t` before it looks at the
        // sequence at all, so one that does not fit raises `OverflowError` —
        // even where the answer is obviously empty, and even when the count is
        // negative. `"" * 2**71` is an error; `"" * -1` is `""`.
        let Some(n) = n.to_i64() else {
            return Some(Err(OpError::Overflow));
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
                let len = a.len().saturating_mul(n);
                if len > MAX_RESULT_LEN {
                    Err(OpError::ResultTooLarge)
                } else if len == 0 {
                    // **An empty result needs no loop, and that is a fix rather
                    // than a tidy-up.** `[] * 10**18` passes the length check —
                    // zero times anything is zero — and then spun through
                    // `0..n` appending nothing, on the one task that owns all
                    // room state. Any authenticated client could stop a room
                    // dead with one `Set`. Found by the CPython vectors once
                    // the operand matrix gained integers large enough to make
                    // the loop visibly not terminate.
                    Ok(Value::Array(Vec::new()))
                } else {
                    let mut out = Vec::with_capacity(len);
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
    match (&x, &y) {
        (PyNum::Int(a), PyNum::Int(b)) => {
            // Before multiplying, not after: the product's width is the sum of
            // the operands' and is known without building it.
            if a.bits().saturating_add(b.bits()) > MAX_INT_BITS {
                return Err(OpError::IntegerTooWide);
            }
            int_value(a * b)
        }
        _ => float_value(&x, &y, |a, b| a * b),
    }
}

fn pow(current: &Value, arg: &Value) -> OpResult {
    let (x, y) = need_nums("**", current, arg)?;
    match (&x, &y) {
        (PyNum::Int(a), PyNum::Int(b)) if !b.is_negative() => {
            let Some(exp) = b.to_u32() else {
                // An exponent too large to hold, which is unbounded for every
                // base except the three that answer instantly. CPython answers
                // those, so refusing them would be a divergence invented for
                // nothing. `bits()` measures the magnitude: 0 for zero, 1 for
                // ±1. The exponent cannot be zero here, so `0**0` is not this
                // case — it goes down the ordinary path and gives 1.
                return match (a.bits(), a.is_negative()) {
                    (0, _) => int_value(BigInt::ZERO),
                    // `(-1)**n` alternates, and `b.bit(0)` is `n` being odd.
                    (1, true) if b.bit(0) => int_value(BigInt::from(-1)),
                    (1, _) => int_value(BigInt::from(1)),
                    _ => Err(OpError::IntegerTooWide),
                };
            };
            // `pow(2, 10**9)` is 125 MB, and the reference server allocates it
            // rather than refusing — so this has to be decided from the *size*
            // of the answer, before any of it exists.
            let projected = if a.bits() <= 1 {
                1
            } else {
                a.bits().saturating_mul(u64::from(exp))
            };
            if projected > MAX_INT_BITS {
                return Err(OpError::IntegerTooWide);
            }
            int_value(a.pow(exp))
        }
        // A negative integer exponent produces a float in Python 3.
        _ => float_value(&x, &y, f64::powf),
    }
}

fn modulo(current: &Value, arg: &Value) -> OpResult {
    if matches!(current, Value::String(_)) {
        return Err(OpError::StringFormatting);
    }
    let (x, y) = need_nums("%", current, arg)?;
    match (&x, &y) {
        (PyNum::Int(a), PyNum::Int(b)) => {
            int_value(pyvalue::floor_mod_big(a, b).ok_or(OpError::ZeroDivisionError)?)
        }
        _ => {
            let (a, b) = pyvalue::as_floats(&x, &y).ok_or(OpError::Overflow)?;
            num_value(PyNum::Float(
                pyvalue::floor_mod_f64(a, b).ok_or(OpError::ZeroDivisionError)?,
            ))
        }
    }
}

/// `floor`/`ceil` ignore the argument entirely and return an int.
fn round(current: &Value, f: fn(f64) -> f64) -> OpResult {
    match pyvalue::as_num(current) {
        Some(PyNum::Int(i)) => int_value(i),
        Some(PyNum::Float(x)) => {
            let r = f(x);
            if !r.is_finite() {
                return Err(OpError::NotFinite);
            }
            // Python answers an exact int however large the double was, and a
            // double with no fractional part converts exactly.
            int_value(BigInt::from_f64(r).ok_or(OpError::NotFinite)?)
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

/// `&`, `|`, `^` over Python's integers.
///
/// `num_bigint` gives these two's-complement semantics over a sign-magnitude
/// representation, which is what Python's conceptually-infinite sign extension
/// amounts to: `-1 & 0xff` is `0xff`, not something width-dependent. Getting
/// that right by hand for negative operands is most of why this crate is a
/// dependency rather than four hundred lines here.
fn bitwise(
    op: &'static str,
    current: &Value,
    arg: &Value,
    f: fn(&BigInt, &BigInt) -> BigInt,
) -> OpResult {
    // `True & True` is `True` in Python, not `1`.
    if let (Value::Bool(a), Value::Bool(b)) = (current, arg) {
        let r = f(&BigInt::from(*a as u8), &BigInt::from(*b as u8));
        return Ok(Value::Bool(!r.is_zero()));
    }
    match need_nums(op, current, arg)? {
        (PyNum::Int(a), PyNum::Int(b)) => int_value(f(&a, &b)),
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
    match need_nums(op, current, arg)? {
        (PyNum::Int(a), PyNum::Int(b)) => {
            if b.is_negative() {
                return Err(OpError::ValueError("negative shift count".into()));
            }
            if left {
                // Decided before shifting, for the reason `pow` gives: the
                // width of `1 << 10**9` is known from the count alone, and
                // building it is the denial of service.
                let count = b.to_u64().ok_or(OpError::IntegerTooWide)?;
                if a.bits().saturating_add(count) > MAX_INT_BITS {
                    return Err(OpError::IntegerTooWide);
                }
                int_value(a << count)
            } else {
                // Python's `>>` is arithmetic, so shifting past the width
                // saturates toward the sign rather than to zero — and a count
                // too large to hold is simply "past the width".
                let Some(count) = b.to_u64() else {
                    return int_value(if a.is_negative() {
                        BigInt::from(-1)
                    } else {
                        BigInt::ZERO
                    });
                };
                int_value(a >> count)
            }
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
            // An index too wide for an `i64` is out of range either way, and
            // the asymmetry below decides which way that lands.
            let Some(i) = i.to_i64() else {
                return if i.is_negative() {
                    Err(OpError::IndexError)
                } else {
                    Ok(Value::Array(items))
                };
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
