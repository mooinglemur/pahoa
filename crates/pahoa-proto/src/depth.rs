//! Nesting-depth guard for inbound JSON.
//!
//! Archipelago rejects incoming documents nested deeper than 16
//! (`NetUtils.py:176-205`, added by "Core: limit depth of received JSON to 16").
//! It is undocumented but enforced, and a client that trips it gets its socket
//! closed rather than an `InvalidPacket`.
//!
//! This runs as a byte scan *before* the JSON parser sees the frame, mirroring
//! Python's `_check_depth`. That ordering is the point: a hostile deeply-nested
//! frame never reaches serde_json at all. serde_json's own recursion limit is
//! 128 and can only be changed via an `unsafe` escape hatch, so it is no help
//! here anyway.

/// Archipelago's limit (`NetUtils.py:177`).
pub const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DepthError {
    #[error("JSON document too complex (nested deeper than {limit})")]
    TooDeep { limit: usize },
    #[error("JSON document malformed (unbalanced brackets)")]
    Malformed,
}

/// Check nesting depth without parsing.
///
/// Tracks string literals and escapes so brackets inside strings do not count,
/// which is exactly what Python's version does.
pub fn check_depth(s: &str, limit: usize) -> Result<(), DepthError> {
    // Signed, because Python's counter is allowed to go negative and only the
    // final total is checked. `]]][[[` therefore ends back at 1 and passes
    // there; saturating at zero here would reject it, and gratuitous
    // disagreement on malformed input is still disagreement.
    let mut depth: i64 = 1;
    let limit_i = limit as i64;
    let mut in_quotes = false;
    let mut escape = false;

    for c in s.bytes() {
        if c == b'\\' && !escape {
            escape = true;
            continue;
        }
        if c == b'"' && !escape {
            in_quotes = !in_quotes;
            // Python `continue`s here, skipping its `escape = False`; escape is
            // already false on this path, so the effect is the same.
            continue;
        }
        if !in_quotes {
            match c {
                b'[' | b'{' => {
                    depth += 1;
                    if depth > limit_i {
                        return Err(DepthError::TooDeep { limit });
                    }
                }
                b']' | b'}' => depth -= 1,
                _ => {}
            }
        }
        escape = false;
    }

    if depth != 1 {
        return Err(DepthError::Malformed);
    }
    Ok(())
}

/// Check against Archipelago's limit.
pub fn check(s: &str) -> Result<(), DepthError> {
    check_depth(s, MAX_DEPTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_flat_and_shallow_documents() {
        check("[]").unwrap();
        check(r#"[{"cmd":"Sync"}]"#).unwrap();
        check(r#"[{"cmd":"Set","operations":[{"operation":"add","value":1}]}]"#).unwrap();
    }

    #[test]
    fn counts_the_outermost_container() {
        // Python starts at depth 1 and increments on every opener, so a
        // document with 15 nested arrays inside the top-level one is the limit.
        let ok = "[".repeat(15) + &"]".repeat(15);
        check(&ok).unwrap();
        let too_deep = "[".repeat(16) + &"]".repeat(16);
        assert_eq!(check(&too_deep), Err(DepthError::TooDeep { limit: 16 }));
    }

    #[test]
    fn brackets_inside_strings_do_not_count() {
        // A client sending a chat message full of brackets must not be cut off.
        let s = format!(r#"[{{"cmd":"Say","text":"{}"}}]"#, "[".repeat(100));
        check(&s).unwrap();
    }

    #[test]
    fn escaped_quotes_do_not_end_the_string() {
        // Mishandling this would treat the rest of the payload as structure.
        let s = r#"[{"text":"he said \"[[[\" and left"}]"#;
        check(s).unwrap();
    }

    #[test]
    fn escaped_backslash_before_a_quote_still_closes_it() {
        // "a\\" is a complete string ending in one backslash; the following
        // brackets are real structure.
        let s = r#"[{"text":"a\\"},[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]"#;
        assert_eq!(check(s), Err(DepthError::TooDeep { limit: 16 }));
    }

    #[test]
    fn rejects_unbalanced_documents() {
        assert_eq!(check("[[]"), Err(DepthError::Malformed));
        assert_eq!(check("[]]"), Err(DepthError::Malformed));
    }

    #[test]
    fn matches_pythons_negative_counter_on_malformed_input() {
        // Python's depth counter is unbounded below and only the final total is
        // checked, so closers-then-openers balances back to 1 and passes this
        // stage (the JSON parser rejects it a moment later). Clamping at zero
        // would disagree here for no benefit.
        check("]]][[[").unwrap();
    }

    #[test]
    fn mixed_brackets_and_braces_share_one_counter() {
        let s = "[{".repeat(8) + &"}]".repeat(8);
        // 16 openers past the implicit start.
        assert_eq!(check(&s), Err(DepthError::TooDeep { limit: 16 }));
    }
}
