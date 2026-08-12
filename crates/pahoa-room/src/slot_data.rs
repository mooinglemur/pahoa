//! Rendering `slot_data` to JSON, verbatim.
//!
//! `slot_data` is opaque per-world state that the server forwards to clients
//! untouched. It cannot go through `serde_json::Value` because a real seed
//! carries an integer larger than `u64` (`slot_data[…]["seed_name"] ==
//! 56979137468180783661`), and `Value`'s number type cannot hold it without
//! enabling `arbitrary_precision` globally — which would change how *all* JSON
//! in the server parses, for the sake of one field.
//!
//! So this writes JSON text directly and hands back a [`RawValue`], which
//! serde_json emits byte for byte. Exact digits survive, and nothing else in
//! the codebase pays for it.

use pahoa_pickle::PyObj;
use serde_json::value::RawValue;

/// Render a `slot_data` value as raw JSON.
pub fn to_json(value: &PyObj) -> Box<RawValue> {
    let mut buf = String::new();
    write(value, &mut buf);
    RawValue::from_string(buf).expect("slot_data rendering always produces valid JSON")
}

fn write(value: &PyObj, out: &mut String) {
    match value {
        PyObj::None => out.push_str("null"),
        PyObj::Bool(true) => out.push_str("true"),
        PyObj::Bool(false) => out.push_str("false"),
        PyObj::Int(i) => out.push_str(&i.to_string()),
        // The whole reason this module exists.
        PyObj::Big(b) => out.push_str(&b.to_string()),
        PyObj::Float(f) => out.push_str(&float(*f)),
        PyObj::Str(s) => escape(s, out),
        PyObj::List(items) | PyObj::Tuple(items) | PyObj::Set(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        PyObj::Dict(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                key(k, out);
                out.push(':');
                write(v, out);
            }
            out.push('}');
        }
        // Generation converts slot_data to base types before pickling
        // (`Main.py:355-356`), so a class instance here means the seed is not
        // what we think it is. Render it visibly rather than silently dropping.
        PyObj::Instance { class, .. } => {
            escape(&format!("<unrepresentable {class}>"), out);
        }
        PyObj::Global(class) => {
            escape(&format!("<unrepresentable {class}>"), out);
        }
    }
}

/// JSON object keys must be strings, and Python's encoder coerces them.
///
/// `json.dumps({1: 2})` yields `{"1": 2}`, and `True`/`None` become `"true"`
/// and `"null"` — not `"True"`/`"None"`.
fn key(k: &PyObj, out: &mut String) {
    match k {
        PyObj::Str(s) => escape(s, out),
        PyObj::Int(i) => escape(&i.to_string(), out),
        PyObj::Big(b) => escape(&b.to_string(), out),
        PyObj::Bool(true) => escape("true", out),
        PyObj::Bool(false) => escape("false", out),
        PyObj::None => escape("null", out),
        PyObj::Float(f) => escape(&float(*f), out),
        other => escape(&format!("<unrepresentable {}>", other.type_name()), out),
    }
}

/// **Known divergence.** Python's `json` emits bare `Infinity`/`-Infinity`/`NaN`
/// tokens by default (`allow_nan=True`); those are not valid JSON, and emitting
/// them would corrupt the *entire frame* for every client receiving it, not just
/// this one field. `null` is substituted instead.
///
/// The trade is deliberate: a world storing a non-finite float loses that value,
/// versus every client on the room failing to parse a packet. No known world
/// does this, but slot_data is world-controlled, so the case has to be decided
/// rather than left to chance.
///
/// Finite floats use shortest round-trip formatting, as does Python's `repr`.
fn float(f: f64) -> String {
    serde_json::Number::from_f64(f)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn escape(s: &str, out: &mut String) {
    // Delegate escaping so it matches the rest of our output exactly.
    let encoded = serde_json::to_string(s).expect("strings always encode");
    out.push_str(&encoded);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(v: &PyObj) -> String {
        to_json(v).get().to_string()
    }

    fn s(v: &str) -> PyObj {
        PyObj::Str(v.into())
    }

    #[test]
    fn renders_scalars() {
        assert_eq!(render(&PyObj::None), "null");
        assert_eq!(render(&PyObj::Bool(true)), "true");
        assert_eq!(render(&PyObj::Int(-5)), "-5");
        assert_eq!(render(&s("hi")), r#""hi""#);
    }

    #[test]
    fn preserves_integers_wider_than_u64() {
        // The value that actually appears in a live seed. Going through
        // serde_json::Value would truncate or reject it.
        let pickled = b"\x80\x04\x8a\x09\x2d\xe2\x10\x8f\xa3\x8f\xbe\x16\x03.";
        let v = pahoa_pickle::from_slice(pickled, &pahoa_pickle::Allowlist::archipelago()).unwrap();
        assert_eq!(render(&v), "56979137468180783661");
    }

    #[test]
    fn preserves_dict_insertion_order() {
        // Worlds index slot_data by key, but order is still observable to
        // anything that hashes or diffs the payload.
        let d = PyObj::Dict(vec![
            (s("z"), PyObj::Int(1)),
            (s("a"), PyObj::Int(2)),
            (s("m"), PyObj::Int(3)),
        ]);
        assert_eq!(render(&d), r#"{"z":1,"a":2,"m":3}"#);
    }

    #[test]
    fn coerces_non_string_keys_the_way_python_does() {
        // json.dumps({1: "a", True: "b", None: "c"}) -> {"1":"a","true":"b","null":"c"}
        let d = PyObj::Dict(vec![
            (PyObj::Int(1), s("a")),
            (PyObj::Bool(true), s("b")),
            (PyObj::None, s("c")),
        ]);
        assert_eq!(render(&d), r#"{"1":"a","true":"b","null":"c"}"#);
    }

    #[test]
    fn tuples_and_sets_become_arrays() {
        assert_eq!(
            render(&PyObj::Tuple(vec![PyObj::Int(1), PyObj::Int(2)])),
            "[1,2]"
        );
        assert_eq!(render(&PyObj::Set(vec![PyObj::Int(3)])), "[3]");
    }

    #[test]
    fn escapes_strings_and_keeps_non_ascii_literal() {
        assert_eq!(render(&s("a\"b\\c\nd")), r#""a\"b\\c\nd""#);
        // ensure_ascii=False on the Python side.
        assert_eq!(render(&s("✓")), "\"✓\"");
    }

    #[test]
    fn nested_structures_round_trip_through_serde() {
        let v = PyObj::Dict(vec![(
            s("outer"),
            PyObj::List(vec![PyObj::Dict(vec![(s("inner"), PyObj::Bool(false))])]),
        )]);
        let raw = render(&v);
        assert_eq!(raw, r#"{"outer":[{"inner":false}]}"#);
        // And it is valid JSON, which RawValue does not itself guarantee.
        serde_json::from_str::<serde_json::Value>(&raw).unwrap();
    }

    #[test]
    fn non_finite_floats_become_null_rather_than_invalid_json() {
        // Known divergence: Python emits bare Infinity/NaN tokens, which would
        // make the whole frame unparseable for every recipient.
        assert_eq!(render(&PyObj::Float(f64::INFINITY)), "null");
        assert_eq!(render(&PyObj::Float(f64::NEG_INFINITY)), "null");
        assert_eq!(render(&PyObj::Float(f64::NAN)), "null");

        // Finite floats are unaffected.
        assert_eq!(render(&PyObj::Float(1.5)), "1.5");
    }
}
