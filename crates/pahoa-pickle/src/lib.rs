//! A reader for the Python pickle subset used by Archipelago's `.archipelago`
//! multidata and `.apsave` save files.
//!
//! # Why not `serde-pickle`
//!
//! `serde-pickle` decodes unknown classes by pushing only their argument tuple
//! and discarding class identity, so `Hint` and `NetworkItem` both arrive as
//! bare tuples. This crate keeps identity ([`ClassId`]), which buys precise
//! errors and lets callers detect multidata shape changes across Archipelago
//! releases instead of silently mis-parsing positionally.
//!
//! # Scope
//!
//! Deliberately closed: exactly the 33 opcodes observed across real multidata,
//! all protocol 4, plus an [`Allowlist`] of permitted classes mirroring
//! `Utils.restricted_loads`. Valid-but-unsupported opcodes are reported by name.
//!
//! ```no_run
//! use pahoa_pickle::{Allowlist, from_slice};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let raw: Vec<u8> = std::fs::read("multidata.pickle")?;
//! let value = from_slice(&raw, &Allowlist::archipelago())?;
//! println!("{:?}", value.get("seed_name"));
//! # Ok(())
//! # }
//! ```

mod allowlist;
mod bigint;
mod canonical;
mod error;
mod reader;
mod value;

pub use allowlist::Allowlist;
pub use bigint::BigInt;
pub use canonical::canonical;
pub use error::{Error, Result};
pub use reader::{MAX_OBJECTS, Reader};
pub use value::{ClassId, PyObj};

/// Decode a pickle stream under the given class allowlist.
pub fn from_slice(buf: &[u8], allowlist: &Allowlist) -> Result<PyObj> {
    Reader::new(buf, allowlist).decode()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handwritten protocol-4 streams, so the reader is exercised without
    /// needing a fixture on disk.
    fn decode(bytes: &[u8]) -> Result<PyObj> {
        from_slice(bytes, &Allowlist::archipelago())
    }

    #[test]
    fn decodes_scalars() {
        assert_eq!(decode(b"\x80\x04N.").unwrap(), PyObj::None);
        assert_eq!(decode(b"\x80\x04\x88.").unwrap(), PyObj::Bool(true));
        assert_eq!(decode(b"\x80\x04\x89.").unwrap(), PyObj::Bool(false));
        assert_eq!(decode(b"\x80\x04K\x07.").unwrap(), PyObj::Int(7));
        assert_eq!(decode(b"\x80\x04M\x00\x01.").unwrap(), PyObj::Int(256));
        assert_eq!(
            decode(b"\x80\x04J\xff\xff\xff\xff.").unwrap(),
            PyObj::Int(-1)
        );
    }

    #[test]
    fn decodes_binfloat_as_big_endian() {
        // 1.5 is 0x3FF8000000000000; BINFLOAT is the one big-endian opcode.
        let v = decode(b"\x80\x04G\x3f\xf8\x00\x00\x00\x00\x00\x00.").unwrap();
        assert_eq!(v, PyObj::Float(1.5));
    }

    #[test]
    fn decodes_short_and_long_unicode() {
        assert_eq!(
            decode(b"\x80\x04\x8c\x02hi.").unwrap(),
            PyObj::Str("hi".into())
        );
        assert_eq!(
            decode(b"\x80\x04X\x02\x00\x00\x00hi.").unwrap(),
            PyObj::Str("hi".into())
        );
        // Non-ASCII must survive; AP slot names are user-supplied.
        assert_eq!(
            decode(b"\x80\x04\x8c\x03\xe2\x9c\x93.").unwrap(),
            PyObj::Str("✓".into())
        );
    }

    #[test]
    fn decodes_containers() {
        // (1, 2)
        assert_eq!(
            decode(b"\x80\x04K\x01K\x02\x86.").unwrap(),
            PyObj::Tuple(vec![PyObj::Int(1), PyObj::Int(2)])
        );
        // [1, 2] via EMPTY_LIST + MARK + APPENDS
        assert_eq!(
            decode(b"\x80\x04](K\x01K\x02e.").unwrap(),
            PyObj::List(vec![PyObj::Int(1), PyObj::Int(2)])
        );
        // {1: 2} via EMPTY_DICT + SETITEM
        assert_eq!(
            decode(b"\x80\x04}K\x01K\x02s.").unwrap(),
            PyObj::Dict(vec![(PyObj::Int(1), PyObj::Int(2))])
        );
        // {1} via EMPTY_SET + MARK + ADDITEMS
        assert_eq!(
            decode(b"\x80\x04\x8f(K\x01\x90.").unwrap(),
            PyObj::Set(vec![PyObj::Int(1)])
        );
    }

    #[test]
    fn dict_preserves_insertion_order() {
        // Python dicts are ordered and that order is observable downstream,
        // so the reader must not sort or dedupe.
        let v = decode(b"\x80\x04}(\x8c\x01bK\x01\x8c\x01aK\x02u.").unwrap();
        let keys: Vec<_> = v
            .as_dict()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str().unwrap())
            .collect();
        assert_eq!(keys, ["b", "a"]);
    }

    #[test]
    fn memoization_round_trips() {
        // Memoize "hi", then fetch it back: ("hi", "hi")
        let v = decode(b"\x80\x04\x8c\x02hi\x94h\x00\x86.").unwrap();
        assert_eq!(
            v,
            PyObj::Tuple(vec![PyObj::Str("hi".into()), PyObj::Str("hi".into())])
        );
    }

    #[test]
    fn shared_mutable_containers_see_later_mutations() {
        // `pickle.dumps([shared, shared])` where `shared = {"a": 1}`.
        //
        // Pickle memoizes the dict while it is still EMPTY, fills it, and only
        // then fetches it back with BINGET. A reader that snapshots the value at
        // MEMOIZE time silently yields `[{"a": 1}, {}]` — the second reference
        // frozen at its empty state. CPython's memo holds a reference, so both
        // entries are the same filled dict.
        let stream = b"\x80\x04\x95\x10\x00\x00\x00\x00\x00\x00\x00\x5d\x94\x28\x7d\x94\x8c\x01\x61\x94\x4b\x01\x73\x68\x01\x65\x2e";
        let v = decode(stream).unwrap();
        let items = v.as_seq().expect("a list");
        assert_eq!(items.len(), 2);
        let expected = PyObj::Dict(vec![(PyObj::Str("a".into()), PyObj::Int(1))]);
        assert_eq!(items[0], expected, "first reference");
        assert_eq!(
            items[1], expected,
            "second reference must not be the empty snapshot"
        );
    }

    #[test]
    fn shared_containers_survive_being_consumed_by_a_parent() {
        // The aliased list is appended into a parent (leaving the stack) and
        // only afterwards fetched again, so the memo must have materialized it
        // rather than left a dangling reference.
        // `{"x": [shared], "y": shared}` with `shared = [7]`.
        let mut py = Vec::new();
        py.extend_from_slice(b"\x80\x04}\x94"); // EMPTY_DICT, MEMOIZE(0)
        py.extend_from_slice(b"("); // MARK
        py.extend_from_slice(b"\x8c\x01x\x94"); // "x", MEMOIZE(1)
        py.extend_from_slice(b"]\x94"); // EMPTY_LIST outer, MEMOIZE(2)
        py.extend_from_slice(b"]\x94"); // EMPTY_LIST shared, MEMOIZE(3)
        py.extend_from_slice(b"K\x07a"); // 7, APPEND -> shared == [7]
        py.extend_from_slice(b"a"); // APPEND shared into outer; shared leaves the stack
        py.extend_from_slice(b"\x8c\x01y\x94"); // "y", MEMOIZE(4)
        py.extend_from_slice(b"h\x03"); // BINGET(3) -> shared
        py.extend_from_slice(b"u."); // SETITEMS, STOP

        let v = decode(&py).unwrap();
        let shared = PyObj::List(vec![PyObj::Int(7)]);
        assert_eq!(
            v.get("y"),
            Some(&shared),
            "aliased list must be fully populated"
        );
        assert_eq!(v.get("x"), Some(&PyObj::List(vec![shared])));
    }

    #[test]
    fn builds_enum_via_reduce() {
        // NetUtils.SlotType(1) — STACK_GLOBAL, arg, TUPLE1, REDUCE
        let v = decode(b"\x80\x04\x8c\x08NetUtils\x8c\x08SlotType\x93K\x01\x85R.").unwrap();
        assert_eq!(
            v.as_instance_of("NetUtils", "SlotType").unwrap(),
            &[PyObj::Int(1)]
        );
    }

    #[test]
    fn builds_namedtuple_via_newobj() {
        // NetUtils.NetworkSlot.__new__(cls, "name", "game", 1, ())
        let v = decode(
            b"\x80\x04\x8c\x08NetUtils\x8c\x0bNetworkSlot\x93(\x8c\x01n\x8c\x01gK\x01)t\x81.",
        )
        .unwrap();
        let args = v.as_instance_of("NetUtils", "NetworkSlot").unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0].as_str(), Some("n"));
        assert_eq!(args[2].as_int(), Some(1));
    }

    #[test]
    fn refuses_classes_outside_the_allowlist() {
        // The classic pickle RCE gadget must be refused at the class reference,
        // before any REDUCE can name it.
        let err = decode(b"\x80\x04\x8c\x02os\x8c\x06system\x93\x8c\x02ls\x85R.").unwrap_err();
        assert!(
            matches!(&err, Error::ForbiddenClass { class, .. } if class.matches("os", "system")),
            "expected ForbiddenClass, got {err}"
        );
    }

    #[test]
    fn reports_unsupported_opcodes_by_name() {
        // GLOBAL (text form) is valid pickle we refuse on principle.
        let err = decode(b"\x80\x04cos\nsystem\n.").unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedOpcode { name: "GLOBAL", .. }),
            "expected named UnsupportedOpcode, got {err}"
        );
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(matches!(
            decode(b"\x80\x04\x8c\x05hi"),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_stream_without_stop() {
        assert!(matches!(
            decode(b"\x80\x04K\x01"),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_unbalanced_stack_at_stop() {
        assert!(matches!(
            decode(b"\x80\x04K\x01K\x02."),
            Err(Error::UnbalancedStack { len: 2 })
        ));
    }

    #[test]
    fn rejects_bad_memo_index() {
        assert!(matches!(
            decode(b"\x80\x04h\x05."),
            Err(Error::BadMemo { index: 5, .. })
        ));
    }
}
