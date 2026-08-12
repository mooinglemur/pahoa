//! Replays every data-storage operation against CPython's recorded behavior.
//!
//! The vectors come from Archipelago's own `MultiServer.modify_functions` (see
//! `tools/gen-datastore-vectors.py`), so this is a comparison against the real
//! implementation rather than against a second reading of it.
//!
//! Divergences are *enumerated*, not tolerated in general: a case where pahoa
//! errors and CPython succeeded is only accepted if it falls in one of the four
//! documented buckets, and the test prints the tally so a silent drift in that
//! count is visible.

use pahoa_datastore::{OpError, apply};
use serde_json::Value;

const VECTORS: &str = include_str!("vectors.jsonl");

#[derive(Debug)]
struct Case {
    op: String,
    current: Value,
    arg: Value,
    expected: Expected,
}

#[derive(Debug)]
enum Expected {
    Value(Value),
    /// CPython raised; the name of the exception.
    Raised(String),
    /// Not representable in JSON on either side.
    Skipped,
}

fn parse() -> Vec<Case> {
    VECTORS
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let v: Value = serde_json::from_str(line).expect("vector line is JSON");
            let expected = if let Some(e) = v.get("error") {
                Expected::Raised(e.as_str().unwrap().to_string())
            } else if v.get("skip").is_some() {
                Expected::Skipped
            } else {
                Expected::Value(v["result"].clone())
            };
            Case {
                op: v["op"].as_str().unwrap().to_string(),
                current: v["current"].clone(),
                arg: v["arg"].clone(),
                expected,
            }
        })
        .collect()
}

/// The documented divergences, all of which turn a Python success into a pahoa
/// error. Anything else is a real mismatch.
///
/// Three are denial-of-service bounds — Python's unbounded integers and
/// sequences make `pow(2, 10**9)` and `"x" * 10**9` remote memory exhaustion.
/// The fourth is printf-style string formatting via `mod`; see [`OpError`].
fn is_documented_divergence(e: &OpError) -> bool {
    matches!(
        e,
        OpError::Overflow
            | OpError::ResultTooLarge
            | OpError::NotFinite
            | OpError::StringFormatting
    )
}

#[test]
fn the_vector_file_covers_every_operation() {
    let cases = parse();
    assert!(
        cases.len() > 10_000,
        "expected the full matrix, got {}",
        cases.len()
    );

    let mut ops: Vec<&str> = cases.iter().map(|c| c.op.as_str()).collect();
    ops.sort_unstable();
    ops.dedup();
    assert_eq!(ops.len(), 18, "expected all 18 operations, got {ops:?}");
}

#[test]
fn every_operation_matches_cpython() {
    let cases = parse();

    let mut matched = 0usize;
    let mut both_failed = 0usize;
    let mut skipped = 0usize;
    let mut divergences: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for case in &cases {
        let got = apply(&case.op, case.current.clone(), &case.arg);

        match (&case.expected, &got) {
            (Expected::Skipped, _) => skipped += 1,

            (Expected::Value(want), Ok(have)) => {
                if have == want {
                    matched += 1;
                } else {
                    failures.push(format!(
                        "{}({}, {}) = {} but CPython gives {}",
                        case.op, case.current, case.arg, have, want
                    ));
                }
            }

            (Expected::Value(want), Err(e)) => {
                if is_documented_divergence(e) {
                    divergences.push(format!(
                        "{}({}, {}): {e} (CPython gives {want})",
                        case.op, case.current, case.arg
                    ));
                } else {
                    failures.push(format!(
                        "{}({}, {}) failed with {e} but CPython gives {want}",
                        case.op, case.current, case.arg
                    ));
                }
            }

            // Both refused. The exception *names* need not match — the room
            // turns every one of them into the same outcome, a dropped
            // connection — but refusing where CPython refused does matter.
            (Expected::Raised(_), Err(_)) => both_failed += 1,

            (Expected::Raised(why), Ok(have)) => {
                failures.push(format!(
                    "{}({}, {}) = {have} but CPython raises {why}",
                    case.op, case.current, case.arg
                ));
            }
        }
    }

    eprintln!(
        "{matched} matched, {both_failed} both refused, {skipped} skipped, \
         {} documented divergences",
        divergences.len()
    );
    for d in divergences.iter().take(10) {
        eprintln!("  divergence: {d}");
    }

    assert!(
        failures.is_empty(),
        "{} mismatch(es) against CPython:\n{}",
        failures.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Guards against the suite quietly becoming a no-op.
    assert!(
        matched > 3000,
        "only {matched} cases actually produced a value"
    );

    // Pin the divergence count. Growth means a new divergence crept in without
    // being reasoned about, which is exactly what this file exists to prevent.
    assert_eq!(
        divergences.len(),
        71,
        "divergence count changed; every entry must be a deliberate, documented decision"
    );
}

/// Spot checks for the traps most likely to be "fixed" by a well-meaning edit.
/// Each of these is a case where the obvious Rust translation is wrong.
mod traps {
    use super::*;
    use serde_json::json;

    fn ok(op: &str, current: Value, arg: Value) -> Value {
        apply(op, current, &arg).expect("should succeed")
    }

    #[test]
    fn modulo_follows_the_divisor_not_the_dividend() {
        // Rust's % would give -1 and 1 here.
        assert_eq!(ok("mod", json!(-7), json!(3)), json!(2));
        assert_eq!(ok("mod", json!(7), json!(-3)), json!(-2));
    }

    #[test]
    fn booleans_participate_as_integers() {
        assert_eq!(ok("add", json!(true), json!(1)), json!(2));
        assert_eq!(ok("add", json!(true), json!(true)), json!(2));
        // ...but bitwise ops on two bools stay bools.
        assert_eq!(ok("and", json!(true), json!(true)), json!(true));
        assert_eq!(ok("or", json!(false), json!(true)), json!(true));
    }

    #[test]
    fn max_and_min_return_the_first_maximal_element() {
        // The int/float distinction survives into the emitted JSON.
        assert_eq!(ok("max", json!(1), json!(1.0)), json!(1));
        assert_eq!(ok("max", json!(1.0), json!(1)), json!(1.0));
        assert_eq!(ok("min", json!(1), json!(1.0)), json!(1));
    }

    #[test]
    fn multiplication_repeats_sequences_in_either_order() {
        assert_eq!(ok("mul", json!("ab"), json!(3)), json!("ababab"));
        assert_eq!(ok("mul", json!(3), json!("ab")), json!("ababab"));
        assert_eq!(ok("mul", json!([1]), json!(2)), json!([1, 1]));
        // A non-positive count empties it, rather than erroring.
        assert_eq!(ok("mul", json!("ab"), json!(0)), json!(""));
        assert_eq!(ok("mul", json!("ab"), json!(-1)), json!(""));
    }

    #[test]
    fn addition_concatenates_and_extends() {
        assert_eq!(ok("add", json!("ab"), json!("c")), json!("abc"));
        assert_eq!(ok("add", json!([1]), json!([2])), json!([1, 2]));
    }

    #[test]
    fn or_merges_dictionaries() {
        // Python 3.9+ dict union; the right-hand side wins.
        assert_eq!(
            ok("or", json!({"a": 1, "b": 2}), json!({"b": 3, "c": 4})),
            json!({"a": 1, "b": 3, "c": 4})
        );
    }

    #[test]
    fn floor_and_ceil_ignore_the_argument_and_return_integers() {
        assert_eq!(ok("floor", json!(2.7), json!("ignored")), json!(2));
        assert_eq!(ok("ceil", json!(2.1), json!(null)), json!(3));
        assert_eq!(ok("floor", json!(-2.1), json!(null)), json!(-3));
    }

    #[test]
    fn update_on_a_list_appends_only_what_is_absent_by_python_equality() {
        assert_eq!(ok("update", json!([1, 2]), json!([2, 3])), json!([1, 2, 3]));
        // 1.0 == 1, so nothing is appended.
        assert_eq!(ok("update", json!([1]), json!([1.0])), json!([1]));
        assert_eq!(ok("update", json!([1]), json!([true])), json!([1]));
    }

    #[test]
    fn update_on_a_list_of_unhashables_is_refused() {
        // Python builds set(container) first, which raises TypeError.
        assert!(apply("update", json!([[1]]), &json!([2])).is_err());
    }

    #[test]
    fn remove_drops_only_the_first_match_and_ignores_absent_values() {
        assert_eq!(ok("remove", json!([1, 2, 1]), json!(1)), json!([2, 1]));
        assert_eq!(ok("remove", json!([1]), json!(9)), json!([1]));
        // Non-lists have no .remove, so Python raises AttributeError.
        assert!(apply("remove", json!({"a": 1}), &json!("a")).is_err());
    }

    #[test]
    fn pop_guards_positive_out_of_range_but_not_negative() {
        // Guarded: a no-op.
        assert_eq!(ok("pop", json!([1, 2]), json!(5)), json!([1, 2]));
        assert_eq!(ok("pop", json!([1, 2]), json!(1)), json!([1]));
        // Negative within range works.
        assert_eq!(ok("pop", json!([1, 2, 3]), json!(-1)), json!([1, 2]));
        // Unguarded: Python raises IndexError and drops the connection.
        assert!(apply("pop", json!([1, 2]), &json!(-5)).is_err());
        // Missing dict keys are guarded.
        assert_eq!(ok("pop", json!({"a": 1}), json!("zz")), json!({"a": 1}));
    }

    #[test]
    fn default_keeps_the_existing_value_and_replace_takes_the_new_one() {
        assert_eq!(ok("default", json!(1), json!(2)), json!(1));
        assert_eq!(ok("replace", json!(1), json!(2)), json!(2));
    }

    #[test]
    fn unknown_operations_are_refused_by_name() {
        match apply("nonsense", json!(1), &json!(1)) {
            Err(OpError::UnknownOperation(name)) => assert_eq!(name, "nonsense"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn the_denial_of_service_shapes_are_bounded() {
        // Python computes these happily and exhausts memory doing it.
        assert!(matches!(
            apply("pow", json!(2), &json!(1_000_000_000)),
            Err(OpError::Overflow)
        ));
        assert!(matches!(
            apply("mul", json!("x"), &json!(1_000_000_000)),
            Err(OpError::ResultTooLarge)
        ));
        assert!(matches!(
            apply("left_shift", json!(1), &json!(1_000_000)),
            Err(OpError::Overflow)
        ));
    }

    #[test]
    fn a_failed_sequence_changes_nothing() {
        use pahoa_datastore::apply_all;
        // The second operation fails; Python would have left the first applied.
        let ops = vec![("add".to_string(), json!(1)), ("mod".to_string(), json!(0))];
        assert!(apply_all(json!(5), &ops).is_err());

        let good = vec![("add".to_string(), json!(1)), ("mul".to_string(), json!(2))];
        assert_eq!(apply_all(json!(5), &good).unwrap(), json!(12));
    }
}
