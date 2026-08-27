//! Work a client asked for that the room had already done.
//!
//! # Why count something the room handles correctly
//!
//! None of this is an error. A repeated `LocationChecks` is filtered against
//! the slot's existing set and a repeated hint is filtered against its hint
//! list, so the room's state and every client's view of it stay right either
//! way. That is exactly the problem: **a client can be badly wrong in a way
//! that produces no wrong behavior, so nothing anywhere says so.**
//!
//! A world's client that re-sends its whole check list on every tick rather
//! than on every reconnect, or that re-scouts the same locations in a loop,
//! costs the room parse and lookup work proportional to its bug and shows up
//! nowhere — not in the log, not in the journal, and not in an error count,
//! because there is no error. It looks like a busy player.
//!
//! Counted per slot, and the metrics layer renders a `game` label beside the
//! slot from the seed's roster, which is the axis that matters here: a bug
//! belongs to a *world's client implementation* rather than to the person
//! running it, so a game whose slots all show the same ratio is a bug report
//! and one slot out of forty is a mod or a script.
//!
//! # Read it as a ratio, never as a threshold
//!
//! **Some redundancy is correct and expected.** A client re-sends its checked
//! locations on reconnect — that is how the protocol resynchronizes, and
//! `register_location_checks` is written to expect it — so every reconnect
//! legitimately contributes, and a room with churn accumulates these without
//! anything being wrong.
//!
//! What distinguishes a bug is the *rate against the slot's own traffic*, which
//! is why this is exported next to `pahoa_packets_total` on the same key: one
//! redundant batch per connection is the protocol working, and a thousand per
//! connection is a client in a loop. A raw count answers neither question.

use crate::SlotKey;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// What was asked for a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// A location the slot had already checked, and which this multidata
    /// knows about.
    ///
    /// Unknown location ids are **not** counted here. Clients legitimately send
    /// ids this seed does not contain, which is a different thing entirely and
    /// not a sign of anything.
    LocationCheck,
    /// A hint the room had already recorded, asked for again through
    /// `CreateHints` or a `LocationScouts` that asked for new hints only.
    Hint,
}

impl Kind {
    pub fn as_text(self) -> &'static str {
        match self {
            Self::LocationCheck => "location_check",
            Self::Hint => "hint",
        }
    }
}

static REDUNDANT: LazyLock<RwLock<HashMap<(SlotKey, Kind), AtomicU64>>> =
    LazyLock::new(RwLock::default);

/// Add `count` redundant requests for one slot. A zero is not recorded, so a
/// slot that has never sent one stays absent rather than appearing as zero.
pub fn record(key: SlotKey, kind: Kind, count: usize) {
    if count == 0 {
        return;
    }
    let count = count as u64;
    // The read path first, which is the common one once a slot has been seen —
    // this runs inside `register_location_checks`, on the actor, and a release
    // pushes every location a slot owns through that function.
    if let Some(slot) = REDUNDANT.read().expect("not poisoned").get(&(key, kind)) {
        slot.fetch_add(count, Ordering::Relaxed);
        return;
    }
    REDUNDANT
        .write()
        .expect("not poisoned")
        .entry((key, kind))
        .or_default()
        .fetch_add(count, Ordering::Relaxed);
}

/// Every observed `(slot, kind)` with its count.
///
/// Observed only: a slot that has never sent a redundant request is absent
/// rather than zero, because a gap and a zero say different things on a
/// dashboard — and on a 2000-slot room the difference is 4000 series.
pub fn by_slot() -> Vec<((SlotKey, Kind), u64)> {
    REDUNDANT
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(key, count)| (*key, count.load(Ordering::Relaxed)))
        .collect()
}

/// The whole room's total for one kind.
pub fn total(kind: Kind) -> u64 {
    REDUNDANT
        .read()
        .expect("not poisoned")
        .iter()
        .filter(|((_, k), _)| *k == kind)
        .map(|(_, count)| count.load(Ordering::Relaxed))
        .sum()
}
