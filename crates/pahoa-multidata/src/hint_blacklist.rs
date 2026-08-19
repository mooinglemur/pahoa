//! Items and locations a world refuses to hint, built into the binary.
//!
//! `World.hint_blacklist` (`worlds/AutoWorld.py:312`) is *"any names that should
//! not be hintable"*. The reference server reads it from the worlds it has
//! installed — `MultiServer.py:343-344` walks `AutoWorldRegister.world_types`
//! into `non_hintable_names` — and `!hint` refuses a match against it with
//! `Sorry, "{name}" is marked as non-hintable.` (`MultiServer.py:1715`, `:1734`).
//!
//! **It is never serialized into multidata by anything.** It is Python class
//! data, and the reference can read it only because it *is* an Archipelago
//! install. A standalone server has to get it from somewhere else, and the two
//! options were an exported JSON snapshot passed in at startup, or this.
//!
//! ## Why built in
//!
//! An external file has three states — present, absent and stale — and a server
//! can only tell the first from the other two. A table compiled into the binary
//! cannot be missing, cannot be stale relative to the code reading it, and
//! cannot be forgotten by whoever deploys the room. That last one is not
//! hypothetical: an orchestrator passing `--snapshot` for a file nothing ever
//! wrote is how the question got asked.
//!
//! The trade, stated once: this tracks the **pahoa build** rather than the
//! apworld that generated the seed, so a world newly adding a `hint_blacklist`
//! needs a pahoa release before it is honored. Against a list that has held two
//! entries across the reference's history, that is the cheaper failure.
//!
//! ## Keeping it current
//!
//! `tools/export-datapackage.py` regenerates this file from an Archipelago
//! checkout and prints what changed, so the table stays derived rather than
//! hand-copied. A game absent from it has an **empty** blacklist, which is
//! exactly what the reference gives a world that does not set one — absence
//! here means "hints everything", not "unknown".

/// Every non-empty `hint_blacklist` in the reference tree, by `World.game`.
///
/// Sorted by game so a regeneration produces a reviewable diff. The names are
/// matched against whichever namespace the hint is for: `alttp`'s entry is an
/// item and `cvcotm`'s is a location, and the reference checks one flat set
/// against both, so this does too.
///
/// Generated from Archipelago; see the module documentation.
pub const HINT_BLACKLIST: &[(&str, &[&str])] = &[
    // worlds/alttp/__init__.py:232
    ("A Link to the Past", &["Triforce"]),
    // worlds/cvcotm/__init__.py:79 — the Battle Arena reward, which is always a
    // Last Key when present.
    (
        "Castlevania - Circle of the Moon",
        &["Battle Arena: End reward"],
    ),
];

/// What `game` refuses to hint. Empty for anything not listed, matching the
/// reference's default of `frozenset()` for a world that sets none.
pub fn for_game(game: &str) -> &'static [&'static str] {
    HINT_BLACKLIST
        .iter()
        .find(|(name, _)| *name == game)
        .map(|(_, names)| *names)
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_worlds_that_set_one_are_present() {
        assert_eq!(for_game("A Link to the Past"), ["Triforce"]);
        assert_eq!(
            for_game("Castlevania - Circle of the Moon"),
            ["Battle Arena: End reward"]
        );
    }

    /// Absence means "hints everything", not "unknown" — the same answer the
    /// reference gives for a world that never sets the field.
    #[test]
    fn an_unlisted_game_blacklists_nothing() {
        assert!(for_game("Balatro").is_empty());
        assert!(for_game("a game that does not exist").is_empty());
    }

    /// Sorted, so regenerating produces a diff a person can read.
    #[test]
    fn the_table_is_sorted_by_game() {
        let names: Vec<&str> = HINT_BLACKLIST.iter().map(|(g, _)| *g).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
}
