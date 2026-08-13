//! Hint storage, the hint economy, and hint ordering.
//!
//! # Storage
//!
//! Hints are kept in a `Vec` per slot, deduplicated on
//! [`pahoa_multidata::HintIdentity`] — the subset of fields Python hashes
//! (`NetUtils.py:418-419`), which excludes `found`, `item_flags` and `status`
//! so that updating a hint's status replaces it in place rather than adding a
//! near-duplicate.
//!
//! A `Vec` rather than a set because insertion order is then deterministic.
//! That is free here, and it keeps our own tests from depending on hash-map
//! iteration order, which Rust randomizes per process.
//!
//! # Ordering, and why it does not match Python exactly
//!
//! Python builds its candidate list by iterating a `set`, so the order handed
//! to `random.shuffle` depends on CPython's set internals — and, because
//! `Hint.__hash__` includes the `entrance` string, on per-process hash
//! randomization. Measured across three `PYTHONHASHSEED` values: hints with
//! empty entrances come out in a stable order (`hash("")` is 0 and is not
//! randomized), but hints carrying entrance names — any entrance-randomized
//! seed — come out differently every run.
//!
//! So Archipelago's own hint order is not reproducible across its own restarts
//! for ER seeds, and "match Python bit-for-bit" is not a reachable target.
//! Reproducing CPython's set iteration would be considerable machinery in
//! pursuit of parity with something that is itself unstable.
//!
//! What is reproduced is everything observable that *is* stable: the shuffle
//! uses CPython's Mersenne Twister seeded from the seed name, the two stable
//! sorts that follow (prefer non-local placements, then prefer earlier
//! spheres), the one-new-hint-per-call rule, and the points arithmetic.

use pahoa_multidata::{Hint, HintIdentity, HintStatus, MultiData, item_flags};
use pahoa_pyrandom::PyRandom;
use std::collections::HashMap;
use std::sync::Arc;

use crate::room::SlotKey;

/// Every slot's hints, in insertion order.
///
/// Each list is behind an `Arc` so a save snapshot is a refcount bump rather
/// than a deep clone — a hint carries an owned entrance string, so cloning the
/// lot would mean one allocation per hint per save.
#[derive(Debug, Default)]
pub struct HintStore {
    by_slot: HashMap<SlotKey, Arc<Vec<Hint>>>,
}

impl HintStore {
    pub fn get(&self, key: SlotKey) -> &[Hint] {
        self.by_slot
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or_default()
    }

    /// The hint covering `location` in `finder`'s world, if one exists.
    ///
    /// Both the receiving and the finding player hold a copy, so a lookup has
    /// to search by `(finding_player, location)` rather than by owner.
    pub fn find(&self, key: SlotKey, finder: u32, location: i64) -> Option<&Hint> {
        self.get(key)
            .iter()
            .find(|h| h.finding_player == finder && h.location == location)
    }

    /// Insert or replace by identity. Returns true if this was new.
    pub fn upsert(&mut self, key: SlotKey, hint: Hint) -> bool {
        let list = Arc::make_mut(self.by_slot.entry(key).or_default());
        let identity = hint.identity();
        match list.iter_mut().find(|h| h.identity() == identity) {
            Some(existing) => {
                *existing = hint;
                false
            }
            None => {
                list.push(hint);
                true
            }
        }
    }

    pub fn contains(&self, key: SlotKey, identity: &HintIdentity) -> bool {
        self.get(key).iter().any(|h| &h.identity() == identity)
    }

    /// Mark hints as found once their location is checked (`Hint.re_check`).
    ///
    /// Returns the slots whose hint lists changed, so only those need notifying.
    pub fn recheck(&mut self, finder: u32, checked: &dyn Fn(u32, i64) -> bool) -> Vec<SlotKey> {
        let mut changed = Vec::new();
        for (key, hints) in self.by_slot.iter_mut() {
            // Which hints move is decided before touching the `Arc`, so a slot
            // with nothing to update is never cloned out from under a save in
            // flight. This runs on every check batch, so it matters.
            let touched: Vec<usize> = hints
                .iter()
                .enumerate()
                .filter(|(_, h)| {
                    h.finding_player == finder
                        && !(h.found && h.status == HintStatus::Found)
                        && checked(h.finding_player, h.location)
                })
                .map(|(i, _)| i)
                .collect();
            if touched.is_empty() {
                continue;
            }
            let hints = Arc::make_mut(hints);
            for i in touched {
                hints[i].found = true;
                hints[i].status = HintStatus::Found;
            }
            changed.push(*key);
        }
        changed.sort_unstable();
        changed
    }

    /// `Hint.re_prioritize`: a found hint cannot be given any other status.
    pub fn set_status(
        &mut self,
        key: SlotKey,
        finder: u32,
        location: i64,
        status: HintStatus,
    ) -> Option<Hint> {
        let list = self.by_slot.get_mut(&key)?;
        // Located before `make_mut`, so a no-op status change never clones.
        let index = list
            .iter()
            .position(|h| h.finding_player == finder && h.location == location)?;
        let status = if list[index].found {
            HintStatus::Found
        } else {
            status
        };
        if list[index].status == status {
            return None;
        }
        let hint = &mut Arc::make_mut(list)[index];
        hint.status = status;
        Some(hint.clone())
    }

    /// Replace a slot's hints wholesale, for save restore.
    pub fn replace(&mut self, key: SlotKey, hints: Vec<Hint>) {
        self.by_slot.insert(key, Arc::new(hints));
    }

    pub fn slots(&self) -> impl Iterator<Item = (&SlotKey, &Arc<Vec<Hint>>)> {
        self.by_slot.iter()
    }
}

/// Which slots a hint request covers: the slot itself plus any item-link group
/// it belongs to, since a group's items are hintable by its members
/// (`MultiServer.py:1189-1192`).
pub fn hintable_slots(data: &MultiData, slot: u32) -> Vec<u32> {
    let mut slots = vec![slot];
    for (group_id, info) in &data.slot_info {
        if info.group_members.contains(&slot) {
            slots.push(*group_id);
        }
    }
    slots.sort_unstable();
    slots.dedup();
    slots
}

/// The status a fresh hint gets when the caller did not specify one.
///
/// Traps default to "avoid" so a hint does not read as an endorsement
/// (`MultiServer.py:1207-1212`); everything else is a priority hint.
pub fn automatic_status(found: bool, flags: u32) -> HintStatus {
    if found {
        HintStatus::Found
    } else if item_flags::is_trap(flags) {
        HintStatus::Avoid
    } else {
        HintStatus::Priority
    }
}

/// Collect hints for every location holding `item` destined for `slot`.
///
/// An existing hint is reused rather than rebuilt, so its status survives.
#[allow(clippy::too_many_arguments)]
pub fn collect_for_item(
    data: &MultiData,
    store: &HintStore,
    key: SlotKey,
    slot: u32,
    item: i64,
    status: Option<HintStatus>,
    checked: &dyn Fn(u32, i64) -> bool,
) -> Vec<Hint> {
    let wanted = hintable_slots(data, slot);
    let mut out = Vec::new();

    // Scanning the flat table is a linear pass over contiguous memory — about
    // 13 MB at 400k locations — rather than a per-slot map lookup.
    for entry in data.locations.all() {
        if entry.item != item || !wanted.contains(&entry.receiver) {
            continue;
        }
        out.push(build(data, store, key, entry, status, checked));
    }
    out
}

/// Collect the hint for one specific location in `slot`'s own world.
pub fn collect_for_location(
    data: &MultiData,
    store: &HintStore,
    key: SlotKey,
    slot: u32,
    location: i64,
    status: Option<HintStatus>,
    checked: &dyn Fn(u32, i64) -> bool,
) -> Vec<Hint> {
    match data.locations.get(slot, location) {
        Some(entry) => vec![build(data, store, key, entry, status, checked)],
        None => Vec::new(),
    }
}

fn build(
    data: &MultiData,
    store: &HintStore,
    key: SlotKey,
    entry: &pahoa_multidata::LocationEntry,
    status: Option<HintStatus>,
    checked: &dyn Fn(u32, i64) -> bool,
) -> Hint {
    if let Some(existing) = store.find(key, entry.sender, entry.location) {
        return existing.clone();
    }
    let found = checked(entry.sender, entry.location);
    let entrance = data
        .er_hint_data
        .get(&entry.sender)
        .and_then(|m| m.get(&entry.location))
        .cloned()
        .unwrap_or_default();

    let status = if found {
        HintStatus::Found
    } else {
        status.unwrap_or_else(|| automatic_status(found, entry.flags))
    };

    Hint {
        receiving_player: entry.receiver,
        finding_player: entry.sender,
        location: entry.location,
        item: entry.item,
        found,
        entrance,
        item_flags: entry.flags,
        status,
    }
}

/// Order candidate hints and take as many as the player can afford.
///
/// Reproduces `MultiServer.py:1774-1790`: shuffle, then two *stable* sorts —
/// first preferring non-local placements, then preferring earlier spheres —
/// and finally take from the end of the list. The sorts must be stable and the
/// second must not reverse ties, or the shuffle's work is undone.
///
/// Returns `(granted, remaining)`.
pub fn choose(
    rng: &mut PyRandom,
    mut candidates: Vec<Hint>,
    sphere_of: &dyn Fn(u32, i64) -> usize,
    budget: usize,
) -> (Vec<Hint>, Vec<Hint>) {
    rng.shuffle(&mut candidates);

    // Prefer hints for items placed in *someone else's* world.
    candidates.sort_by_key(|h| u8::from(h.receiving_player != h.finding_player));
    // Then prefer earlier spheres. Sorted descending and taken from the back,
    // which is how Python arrives at "earliest first".
    candidates.sort_by(|a, b| {
        sphere_of(b.finding_player, b.location).cmp(&sphere_of(a.finding_player, a.location))
    });

    let mut granted = Vec::new();
    for _ in 0..budget {
        match candidates.pop() {
            Some(h) => granted.push(h),
            None => break,
        }
    }
    (granted, candidates)
}

/// Points a slot has to spend (`MultiServer.py:1845-1852`).
pub fn points(checks: usize, check_points: u32, hint_cost: i64, hints_used: i64) -> i64 {
    check_points as i64 * checks as i64 - hint_cost * hints_used
}

/// Python's stand-in for "unlimited" (`MultiServer.py:1766`, `:1770`).
///
/// Reproduced as the literal it is rather than as `usize::MAX`: a hint on an
/// item group with more than a thousand placements really would stop at a
/// thousand in the reference, and that is observable.
pub const UNLIMITED: usize = 1000;

/// How many new hints this call may grant.
///
/// Zero cost means unlimited; otherwise the player gets **one** per invocation
/// even if they could afford several, and hints for already-found locations are
/// free (`MultiServer.py:1765-1771`).
pub fn budget(cost: i64, points_available: i64, any_unfound: bool) -> usize {
    if !any_unfound || cost == 0 {
        UNLIMITED
    } else if points_available / cost > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(recv: u32, find: u32, loc: i64) -> Hint {
        Hint {
            receiving_player: recv,
            finding_player: find,
            location: loc,
            item: 1,
            found: false,
            entrance: String::new(),
            item_flags: 0,
            status: HintStatus::Priority,
        }
    }

    #[test]
    fn upsert_replaces_by_identity_rather_than_duplicating() {
        let mut store = HintStore::default();
        assert!(store.upsert((0, 1), hint(2, 3, 100)));
        assert_eq!(store.get((0, 1)).len(), 1);

        // Same identity, different status: replaces in place.
        let mut updated = hint(2, 3, 100);
        updated.status = HintStatus::Avoid;
        assert!(!store.upsert((0, 1), updated));
        assert_eq!(store.get((0, 1)).len(), 1);
        assert_eq!(store.get((0, 1))[0].status, HintStatus::Avoid);
    }

    #[test]
    fn insertion_order_is_preserved() {
        // Deterministic order is what keeps hint tests from being flaky.
        let mut store = HintStore::default();
        for loc in [300, 100, 200] {
            store.upsert((0, 1), hint(2, 3, loc));
        }
        let locations: Vec<i64> = store.get((0, 1)).iter().map(|h| h.location).collect();
        assert_eq!(locations, [300, 100, 200]);
    }

    #[test]
    fn rechecking_marks_hints_found_once_their_location_is_checked() {
        let mut store = HintStore::default();
        store.upsert((0, 1), hint(1, 5, 100));
        store.upsert((0, 1), hint(1, 5, 200));

        let changed = store.recheck(5, &|_, loc| loc == 100);
        assert_eq!(changed, [(0, 1)]);

        let found: Vec<bool> = store.get((0, 1)).iter().map(|h| h.found).collect();
        assert_eq!(found, [true, false]);
        assert_eq!(store.get((0, 1))[0].status, HintStatus::Found);
    }

    #[test]
    fn a_found_hint_cannot_be_given_another_status() {
        let mut store = HintStore::default();
        let mut h = hint(1, 5, 100);
        h.found = true;
        h.status = HintStatus::Found;
        store.upsert((0, 1), h);

        store.set_status((0, 1), 5, 100, HintStatus::Avoid);
        assert_eq!(store.get((0, 1))[0].status, HintStatus::Found);
    }

    #[test]
    fn traps_default_to_avoid_and_everything_else_to_priority() {
        assert_eq!(automatic_status(false, item_flags::TRAP), HintStatus::Avoid);
        assert_eq!(
            automatic_status(false, item_flags::ADVANCEMENT),
            HintStatus::Priority
        );
        assert_eq!(automatic_status(false, 0), HintStatus::Priority);
        // Found always wins.
        assert_eq!(automatic_status(true, item_flags::TRAP), HintStatus::Found);
    }

    #[test]
    fn points_subtract_the_cost_of_hints_already_taken() {
        assert_eq!(points(10, 1, 5, 0), 10);
        assert_eq!(points(10, 1, 5, 2), 0);
        // Can go negative, which Python allows.
        assert_eq!(points(1, 1, 5, 2), -9);
    }

    #[test]
    fn a_paid_hint_is_limited_to_one_per_call() {
        // Affordable: exactly one, however many points are banked.
        assert_eq!(budget(5, 100, true), 1);
        // Unaffordable: none.
        assert_eq!(budget(5, 4, true), 0);
        // Free hints are unlimited...
        assert_eq!(budget(0, 0, true), UNLIMITED);
        // ...as are hints for locations already found.
        assert_eq!(budget(5, 0, false), UNLIMITED);
    }

    #[test]
    fn ordering_prefers_earlier_spheres_then_non_local_placements() {
        let mut rng = PyRandom::seed_str("ordering");
        // Two local (recv == find) and two remote, spread across spheres.
        let candidates = vec![
            hint(1, 1, 10), // local, sphere 3
            hint(2, 1, 20), // remote, sphere 3
            hint(1, 1, 30), // local, sphere 0
            hint(2, 1, 40), // remote, sphere 0
        ];
        let sphere = |_p: u32, loc: i64| match loc {
            10 | 20 => 3usize,
            _ => 0usize,
        };

        let (granted, _) = choose(&mut rng, candidates, &sphere, 1);
        // Sphere dominates, and within a sphere a remote placement wins.
        assert_eq!(granted.len(), 1);
        assert_eq!(
            granted[0].location, 40,
            "expected the earliest-sphere remote hint"
        );
    }

    #[test]
    fn ordering_is_reproducible_for_the_same_seed() {
        let candidates: Vec<Hint> = (0..20).map(|i| hint(2, 1, i)).collect();
        let sphere = |_: u32, _: i64| 0usize;

        let first = {
            let mut rng = PyRandom::seed_str("stable");
            choose(&mut rng, candidates.clone(), &sphere, 20).0
        };
        let second = {
            let mut rng = PyRandom::seed_str("stable");
            choose(&mut rng, candidates.clone(), &sphere, 20).0
        };
        assert_eq!(first, second, "same seed must give the same order");

        let different = {
            let mut rng = PyRandom::seed_str("other");
            choose(&mut rng, candidates, &sphere, 20).0
        };
        assert_ne!(first, different, "a different seed should reorder");
    }

    #[test]
    fn a_budget_larger_than_the_candidate_pool_takes_everything() {
        let mut rng = PyRandom::seed_str("x");
        let candidates: Vec<Hint> = (0..3).map(|i| hint(2, 1, i)).collect();
        let (granted, remaining) = choose(&mut rng, candidates, &|_, _| 0, usize::MAX);
        assert_eq!(granted.len(), 3);
        assert!(remaining.is_empty());
    }
}
