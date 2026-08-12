//! Canonical rendering, for differential testing against CPython.
//!
//! The rules here are chosen so two independent implementations can hit them
//! exactly. They are not meant to be pretty:
//!
//! - strings are length-prefixed (in UTF-8 bytes) rather than escaped, which
//!   sidesteps any disagreement about escape sequences
//! - floats render as their IEEE-754 bit pattern, sidestepping any
//!   disagreement about shortest-round-trip formatting
//! - sets are sorted by their own rendering, because CPython sets do not
//!   preserve the pickle stream's order while this crate's reader does
//!
//! The Python counterpart is `tools/dump-pickle.py`.

use crate::value::PyObj;
use std::fmt::Write;

pub fn canonical(obj: &PyObj) -> String {
    let mut s = String::new();
    render(obj, &mut s);
    s
}

fn render(obj: &PyObj, out: &mut String) {
    match obj {
        PyObj::None => out.push_str("None"),
        PyObj::Bool(true) => out.push_str("True"),
        PyObj::Bool(false) => out.push_str("False"),
        PyObj::Int(i) => {
            let _ = write!(out, "{i}");
        }
        // Renders identically to a narrow int, matching CPython's single int type.
        PyObj::Big(b) => {
            let _ = write!(out, "{b}");
        }
        PyObj::Float(f) => {
            let _ = write!(out, "f:{:016x}", f.to_bits());
        }
        PyObj::Str(s) => {
            let _ = write!(out, "s{}:{}", s.len(), s);
        }
        PyObj::Tuple(items) => {
            out.push('(');
            join(items, out);
            out.push(')');
        }
        PyObj::List(items) => {
            out.push('[');
            join(items, out);
            out.push(']');
        }
        PyObj::Set(items) => {
            let mut parts: Vec<String> = items.iter().map(canonical).collect();
            parts.sort();
            out.push('{');
            out.push_str(&parts.join(","));
            out.push('}');
        }
        PyObj::Dict(pairs) => {
            out.push_str("d{");
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render(k, out);
                out.push(':');
                render(v, out);
            }
            out.push('}');
        }
        PyObj::Global(c) => {
            let _ = write!(out, "<{c}>");
        }
        PyObj::Instance { class, args } => {
            let _ = write!(out, "<{class}>(");
            join(args, out);
            out.push(')');
        }
    }
}

fn join(items: &[PyObj], out: &mut String) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        render(item, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_scalars_unambiguously() {
        assert_eq!(canonical(&PyObj::None), "None");
        assert_eq!(canonical(&PyObj::Bool(true)), "True");
        assert_eq!(canonical(&PyObj::Int(-5)), "-5");
        assert_eq!(canonical(&PyObj::Str("hi".into())), "s2:hi");
        // Byte length, not char count.
        assert_eq!(canonical(&PyObj::Str("✓".into())), "s3:✓");
    }

    #[test]
    fn renders_floats_by_bit_pattern() {
        assert_eq!(canonical(&PyObj::Float(1.5)), "f:3ff8000000000000");
        // -0.0 and 0.0 compare equal but must render differently.
        assert_ne!(
            canonical(&PyObj::Float(-0.0)),
            canonical(&PyObj::Float(0.0))
        );
    }

    #[test]
    fn sorts_sets_but_preserves_dict_order() {
        let set = PyObj::Set(vec![PyObj::Int(2), PyObj::Int(1)]);
        // Sorted by rendering, so stream order does not leak in.
        assert_eq!(canonical(&set), "{1,2}");

        let dict = PyObj::Dict(vec![
            (PyObj::Str("b".into()), PyObj::Int(1)),
            (PyObj::Str("a".into()), PyObj::Int(2)),
        ]);
        assert_eq!(canonical(&dict), "d{s1:b:1,s1:a:2}");
    }

    #[test]
    fn length_prefix_disambiguates_strings_containing_delimiters() {
        // Without the length prefix these two would render identically.
        let a = PyObj::Tuple(vec![PyObj::Str("x,y".into())]);
        let b = PyObj::Tuple(vec![PyObj::Str("x".into()), PyObj::Str("y".into())]);
        assert_ne!(canonical(&a), canonical(&b));
    }
}
