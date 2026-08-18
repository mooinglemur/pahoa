//! Comparing secrets without leaking them through timing.
//!
//! A plain `==` on a password or a bearer token returns as soon as it finds a
//! differing byte, so how long the comparison took is a measurement of how much
//! of the secret the caller guessed correctly. Over enough attempts that turns
//! an unguessable secret into one recovered a byte at a time. The room's
//! `Connect` handler and the admin API's bearer check are both reachable from
//! the internet and both answer as fast as they can, which is exactly the
//! condition that makes the channel usable.
//!
//! Hand-rolled rather than taking `subtle` as a dependency, on the same terms
//! as the base64 encoder in `pahoa-net`: it is twenty lines, it is testable, and
//! this tree keeps its dependency list short on purpose.

/// Compare two secrets in time that does not depend on **where** they differ.
///
/// The length is not hidden — an unequal length returns early. That is a
/// deliberate limit rather than an oversight: hiding it means hashing both
/// sides first, and the length of a password is not the part worth protecting.
/// What this does hide is the position of the first differing byte, which is
/// the part an attacker can actually walk.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // Accumulate every difference rather than stopping at the first, and hide
    // the accumulator from the optimizer so it cannot reintroduce the early
    // exit this exists to remove.
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// [`ct_eq`] over an optional expected secret.
///
/// `None` means "nothing is required", which is a match. `Some` requires the
/// caller to have supplied one that matches.
pub fn ct_eq_opt(expected: Option<&str>, supplied: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(expected) => match supplied {
            None => false,
            Some(supplied) => ct_eq(expected.as_bytes(), supplied.as_bytes()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_secrets_match() {
        assert!(ct_eq(b"quiet-harbor-ledger", b"quiet-harbor-ledger"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn a_difference_anywhere_fails() {
        assert!(!ct_eq(b"quiet-harbor-ledger", b"quiet-harbor-ledgeR"));
        assert!(!ct_eq(b"quiet-harbor-ledger", b"Quiet-harbor-ledger"));
    }

    #[test]
    fn a_prefix_is_not_a_match() {
        assert!(!ct_eq(b"quiet", b"quiet-harbor"));
        assert!(!ct_eq(b"quiet-harbor", b"quiet"));
    }

    /// `None` and `Some("")` must stay distinguishable: an empty password is not
    /// the same as no password, and the save format is careful about the same
    /// distinction.
    #[test]
    fn no_password_and_an_empty_password_are_different_things() {
        assert!(ct_eq_opt(None, None));
        assert!(ct_eq_opt(None, Some("anything")));
        assert!(!ct_eq_opt(Some(""), None));
        assert!(ct_eq_opt(Some(""), Some("")));
        assert!(!ct_eq_opt(Some(""), Some("x")));
    }

    #[test]
    fn a_required_password_must_be_supplied() {
        assert!(ct_eq_opt(Some("hunter2"), Some("hunter2")));
        assert!(!ct_eq_opt(Some("hunter2"), Some("hunter3")));
        assert!(!ct_eq_opt(Some("hunter2"), None));
    }
}
