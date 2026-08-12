//! The fuzzy matcher must agree with Archipelago's, boundary for boundary.
//!
//! Vectors come from `Utils.get_fuzzy_results` / `get_intended_text` via
//! `tools/gen-fuzzy-vectors.py`, so this also confirms that
//! `strsim::damerau_levenshtein` is the same algorithm jellyfish provides —
//! `strsim` also ships the *restricted* OSA variant, which would differ on
//! some inputs and be very hard to notice.

use pahoa_room::fuzzy::{Match, best, intended, score};
use serde_json::Value;

const VECTORS: &str = include_str!("fuzzy_vectors.jsonl");

struct Case {
    input: String,
    candidates: Vec<String>,
    ranked: Vec<String>,
    scores: Vec<i64>,
    picked: String,
    accepted: bool,
    reason: String,
}

fn parse() -> Vec<Case> {
    VECTORS
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let v: Value = serde_json::from_str(line).expect("vector is JSON");
            let strings = |key: &str| -> Vec<String> {
                v[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect()
            };
            Case {
                input: v["input"].as_str().unwrap().to_string(),
                candidates: strings("candidates"),
                ranked: strings("ranked"),
                scores: v["scores"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_i64().unwrap())
                    .collect(),
                picked: v["picked"].as_str().unwrap().to_string(),
                accepted: v["accepted"].as_bool().unwrap(),
                reason: v["reason"].as_str().unwrap().to_string(),
            }
        })
        .collect()
}

#[test]
fn the_vector_file_is_populated() {
    assert!(parse().len() > 200, "expected the full matrix");
}

#[test]
fn scores_match_archipelago_exactly() {
    let mut checked = 0;
    for case in parse() {
        let refs: Vec<&str> = case.candidates.iter().map(String::as_str).collect();
        let ranked = best(&case.input, &refs, refs.len());

        for (i, (name, got)) in ranked.iter().enumerate() {
            assert_eq!(
                *got, case.scores[i],
                "score for {:?} vs {:?}: got {got}, Archipelago gives {}",
                case.input, name, case.scores[i]
            );
            assert_eq!(
                *name, case.ranked[i],
                "ranking differs for input {:?}: got {ranked:?}, Archipelago gives {:?}",
                case.input, case.ranked
            );
            checked += 1;
        }
    }
    assert!(checked > 500, "only {checked} comparisons ran");
}

#[test]
fn accept_and_reject_decisions_match_archipelago() {
    for case in parse() {
        let refs: Vec<&str> = case.candidates.iter().map(String::as_str).collect();
        let got = intended(&case.input, &refs).expect("candidates are non-empty");

        let (name, accepted, reason) = match &got {
            Match::Accepted { name, reason } => (name.as_str(), true, *reason),
            Match::Rejected { closest, .. } => (closest.as_str(), false, ""),
        };

        assert_eq!(
            accepted,
            case.accepted,
            "input {:?} against {:?}: pahoa {} but Archipelago {} (reason {:?})",
            case.input,
            case.candidates,
            if accepted { "accepted" } else { "rejected" },
            if case.accepted {
                "accepted"
            } else {
                "rejected"
            },
            case.reason,
        );
        assert_eq!(
            name, case.picked,
            "input {:?}: pahoa picked {name:?}, Archipelago picked {:?}",
            case.input, case.picked
        );
        // Only accepted matches carry a reason string on our side; Archipelago
        // reuses that field for its rejection message.
        if accepted {
            assert_eq!(
                reason, case.reason,
                "input {:?}: reason differs",
                case.input
            );
        }
    }
}

#[test]
fn the_exact_match_shortcut_is_distinguishable_from_case_insensitive() {
    // 101 vs 100 is the only thing separating them, and downstream code
    // branches on it.
    assert_eq!(score("Sword", "Sword"), 101);
    assert_eq!(score("sword", "Sword"), 100);
}
