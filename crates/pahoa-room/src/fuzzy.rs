//! Name matching for `!hint`, `!getitem` and the `/send` family.
//!
//! Ports `Utils.get_fuzzy_results` and `Utils.get_intended_text`
//! (`Utils.py:672-724`). The thresholds are player-visible: they decide whether
//! a typo becomes a hint, a "did you mean…", or a refusal, so they are
//! reproduced exactly rather than approximated.
//!
//! Archipelago scores with jellyfish's *unrestricted* Damerau-Levenshtein —
//! the variant that allows a transposed pair to be edited again afterwards.
//! `strsim::damerau_levenshtein` is the same algorithm; `strsim::osa_distance`
//! is the restricted one and gives different answers on inputs like
//! `"ca" -> "abc"`. Using the wrong one would be a subtle, rare, and very
//! confusing divergence.

use strsim::damerau_levenshtein;

/// What `get_intended_text` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Use this name.
    Accepted { name: String, reason: &'static str },
    /// Too uncertain to act on; the text is the message shown to the player.
    Rejected {
        closest: String,
        score: i64,
        message: String,
    },
}

impl Match {
    pub fn accepted(&self) -> Option<&str> {
        match self {
            Match::Accepted { name, .. } => Some(name),
            Match::Rejected { .. } => None,
        }
    }
}

/// `get_fuzzy_ratio`: similarity in `0.0..=1.01`, before truncation.
///
/// An exact match returns 1.01 — above the 1.0 a case-insensitive match can
/// reach — which is how the two are told apart downstream.
pub fn ratio(input: &str, candidate: &str) -> f64 {
    if input == candidate {
        return 1.01;
    }
    let a = input.to_lowercase();
    let b = candidate.to_lowercase();
    let longest = input.chars().count().max(candidate.chars().count());
    if longest == 0 {
        // Two empty strings match above, so only one can be empty here.
        return 0.0;
    }
    1.0 - (damerau_levenshtein(&a, &b) as f64 / longest as f64)
}

/// [`ratio`] scaled to the integer percentage the thresholds are expressed in.
pub fn score(input: &str, candidate: &str) -> i64 {
    // Python's int() truncates toward zero, so a negative ratio — very
    // dissimilar strings — rounds up rather than down.
    (ratio(input, candidate) * 100.0) as i64
}

/// The best `limit` candidates, most similar first.
///
/// Ranking is by the **float** ratio and only then truncated to an integer
/// (`Utils.py:684-693`): Python sorts the `(name, ratio)` pairs and maps
/// `int(ratio * 100)` over the *result*. Sorting by the truncated value instead
/// turns near-misses into ties and reorders them — "Sword" against
/// `["Blue Potion", "Red Potion"]` both score 9, but Red is genuinely the
/// closer of the two and Archipelago ranks it first.
///
/// Python's sort is stable, so a true tie keeps the candidate collection's
/// order. Several call sites in Archipelago pass a `set`, whose iteration order
/// is an implementation detail — this takes a slice and expects the caller to
/// have imposed a deterministic order.
pub fn best<'a>(input: &str, candidates: &[&'a str], limit: usize) -> Vec<(&'a str, i64)> {
    let mut scored: Vec<(&str, f64)> = candidates.iter().map(|c| (*c, ratio(input, c))).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(name, r)| (name, (r * 100.0) as i64))
        .collect()
}

/// `get_intended_text`: decide whether `input` names something.
///
/// The thresholds, from `Utils.py:697-718`:
///
/// | condition | outcome |
/// |---|---|
/// | best == 101 | exact match |
/// | best == 100 | case-insensitive exact match |
/// | best < 75 | rejected, "did you mean…" |
/// | best - second > 5 | close match, accepted |
/// | otherwise | rejected, too many close matches |
///
/// With a single candidate the rule differs: anything above 90 is accepted.
pub fn intended(input: &str, candidates: &[&str]) -> Option<Match> {
    if candidates.is_empty() {
        return None;
    }
    let picks = best(input, candidates, 2);
    let (top, top_score) = picks[0];

    if picks.len() > 1 {
        let difference = top_score - picks[1].1;
        Some(if top_score == 101 {
            Match::Accepted {
                name: top.to_string(),
                reason: "Perfect Match",
            }
        } else if top_score == 100 {
            Match::Accepted {
                name: top.to_string(),
                reason: "Case Insensitive Perfect Match",
            }
        } else if top_score < 75 {
            Match::Rejected {
                closest: top.to_string(),
                score: top_score,
                message: format!(
                    "Didn't find something that closely matches '{input}', \
                     did you mean '{top}'? ({top_score}% sure)"
                ),
            }
        } else if difference > 5 {
            Match::Accepted {
                name: top.to_string(),
                reason: "Close Match",
            }
        } else {
            Match::Rejected {
                closest: top.to_string(),
                score: top_score,
                message: format!(
                    "Too many close matches for '{input}', \
                     did you mean '{top}'? ({top_score}% sure)"
                ),
            }
        })
    } else {
        Some(if top_score > 90 {
            Match::Accepted {
                name: top.to_string(),
                reason: "Only Option Match",
            }
        } else {
            Match::Rejected {
                closest: top.to_string(),
                score: top_score,
                message: format!(
                    "Didn't find something that closely matches '{input}', \
                     did you mean '{top}'? ({top_score}% sure)"
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_match_outranks_a_case_insensitive_one() {
        assert_eq!(score("Sword", "Sword"), 101);
        assert_eq!(score("sword", "Sword"), 100);
    }

    #[test]
    fn a_single_edit_costs_proportionally_to_the_longer_string() {
        // One substitution in a 5-character word: 1 - 1/5 = 0.8.
        assert_eq!(score("sword", "sward"), 80);
    }

    #[test]
    fn completely_different_strings_score_low_or_negative() {
        assert!(score("abc", "xyz") <= 0);
    }

    #[test]
    fn an_exact_match_is_accepted() {
        let m = intended("Sword", &["Sword", "Shield"]).unwrap();
        assert_eq!(m.accepted(), Some("Sword"));
    }

    #[test]
    fn a_clear_typo_is_accepted_as_a_close_match() {
        let m = intended("Swrod", &["Sword", "Completely Different"]).unwrap();
        assert_eq!(m.accepted(), Some("Sword"));
    }

    #[test]
    fn ambiguity_between_similar_names_is_refused() {
        // Equidistant candidates score identically, so the gap is 0 and Python
        // refuses rather than guessing — guessing wrong would spend a hint.
        let m = intended("Swor", &["Sword", "Sworn"]).unwrap();
        assert!(matches!(m, Match::Rejected { .. }), "{m:?}");
    }

    #[test]
    fn nonsense_is_refused_with_the_closest_candidate_named() {
        let m = intended("zzzzzzzz", &["Sword", "Shield"]).unwrap();
        match m {
            Match::Rejected { message, .. } => {
                assert!(message.contains("Didn't find something"), "{message}");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_lone_candidate_needs_to_score_above_ninety() {
        assert!(intended("Sword", &["Sword"]).unwrap().accepted().is_some());
        // A single poor candidate is still refused.
        assert!(intended("zzzz", &["Sword"]).unwrap().accepted().is_none());
    }

    #[test]
    fn no_candidates_yields_nothing_rather_than_a_bogus_match() {
        assert!(intended("Sword", &[]).is_none());
    }

    #[test]
    fn ties_keep_the_candidate_order_they_were_given() {
        // Python's sort is stable, so a tie resolves to whichever candidate
        // came first. Callers must therefore hand over a deterministic order.
        let first = best("x", &["aa", "bb"], 2);
        assert_eq!(first[0].0, "aa");
        let second = best("x", &["bb", "aa"], 2);
        assert_eq!(second[0].0, "bb");
    }
}
