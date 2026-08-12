//! Which classes a pickle stream is permitted to name.
//!
//! This mirrors `Utils.RestrictedUnpickler` in Archipelago
//! (`Utils.py:453-487`). Both multidata and save files are attacker-influenced
//! — datastorage in particular holds arbitrary client-supplied values — so an
//! unrecognized class is refused rather than constructed. Unpickling untrusted
//! data is arbitrary code execution in Python; it is not in Rust, but a closed
//! class set is still the right default and keeps parity with the reference.

use crate::value::ClassId;

#[derive(Debug, Clone)]
pub struct Allowlist {
    entries: Vec<(Box<str>, Box<str>)>,
}

impl Allowlist {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            entries: pairs
                .into_iter()
                .map(|(m, n)| (m.into(), n.into()))
                .collect(),
        }
    }

    pub fn allow(mut self, module: &str, name: &str) -> Self {
        self.entries.push((module.into(), name.into()));
        self
    }

    pub fn permits(&self, class: &ClassId) -> bool {
        self.entries
            .iter()
            .any(|(m, n)| **m == *class.module && **n == *class.name)
    }

    /// The classes Archipelago's own `restricted_loads` permits from
    /// `NetUtils`, which is everything multidata and `.apsave` can contain.
    ///
    /// `Options`/`Plando` classes are deliberately omitted: those appear only on
    /// the WebHost generation path, never in anything a server reads.
    pub fn archipelago() -> Self {
        Self::from_pairs([
            ("NetUtils", "NetworkItem"),
            ("NetUtils", "NetworkSlot"),
            ("NetUtils", "Hint"),
            ("NetUtils", "SlotType"),
            ("NetUtils", "HintStatus"),
            ("NetUtils", "ClientStatus"),
            // Present in save files: `restricted_loads` permits builtins.set /
            // frozenset and collections.Counter. Sets arrive via EMPTY_SET rather
            // than a class reference, so they never reach the allowlist, but
            // Counter would.
            ("collections", "Counter"),
        ])
    }
}

impl Default for Allowlist {
    fn default() -> Self {
        Self::archipelago()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archipelago_permits_the_netutils_classes() {
        let a = Allowlist::archipelago();
        for name in [
            "NetworkItem",
            "NetworkSlot",
            "Hint",
            "SlotType",
            "HintStatus",
            "ClientStatus",
        ] {
            assert!(
                a.permits(&ClassId::new("NetUtils", name)),
                "{name} should be permitted"
            );
        }
    }

    #[test]
    fn archipelago_refuses_arbitrary_classes() {
        let a = Allowlist::archipelago();
        // The canonical pickle RCE gadget.
        assert!(!a.permits(&ClassId::new("os", "system")));
        assert!(!a.permits(&ClassId::new("builtins", "eval")));
        assert!(!a.permits(&ClassId::new("subprocess", "Popen")));
        // Right name, wrong module.
        assert!(!a.permits(&ClassId::new("evil", "NetworkSlot")));
    }
}
