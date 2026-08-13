//! M7's measurement, on the 2000-slot fixture.
//!
//! The plan deferred the journaled-save decision to a number rather than an
//! argument: if a full snapshot is cheap enough on a background thread, a
//! journal buys nothing but failure modes. This is where that number comes
//! from, so it prints its findings whether or not it fails.
//!
//! What is actually asserted is narrower, and is the part that must hold no
//! matter which way the journal decision goes:
//!
//! 1. **`snapshot()` is flat.** Its cost tracks the slot count, not the number
//!    of checks — otherwise every save stalls the actor for as long as the room
//!    has been running, and the whole "disk is write-only" invariant collapses.
//! 2. **A save with one in flight does not stall the room.** Holding a snapshot
//!    makes the next write to each touched slot copy it once; that must stay
//!    proportional to the slots being written, not to the room.
//! 3. **The round trip survives at scale**, which is the kill -9 case.

mod common;

use common::*;
use pahoa_room::save::Snapshot;
use pahoa_room::{Counter, RoomOptions};
use std::time::Instant;

const FIXTURE: &str = "SYNTH_2000slot.archipelago";

#[test]
fn a_full_save_is_cheap_enough_to_decide_the_journal_question() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let slots: Vec<u32> = data.player_slots().map(|(s, _)| *s).collect();
    let mut room = room_for(data.clone(), RoomOptions::default());

    // An empty room first, so the flatness claim has a baseline that predates
    // any checks at all.
    let empty_snapshot = time(100, || {
        std::hint::black_box(room.snapshot());
    });

    let mut sink = Counter::default();
    for &slot in &slots {
        let locations: Vec<i64> = data
            .locations
            .for_slot(slot)
            .iter()
            .map(|e| e.location)
            .collect();
        room.register_location_checks((0, slot), &locations, &mut sink);
    }

    let checks: usize = slots.iter().map(|s| room.checked_count((0, *s))).sum();
    let items: usize = room
        .snapshot()
        .received_items
        .iter()
        .map(|(_, q)| q.len())
        .sum();
    assert!(checks > 100_000, "want a large room, got {checks} checks");

    // (1) Flat: a room holding 340k checks must snapshot in about the time an
    // empty one does, because nothing bulky is copied.
    let full_snapshot = time(100, || {
        std::hint::black_box(room.snapshot());
    });

    let snapshot = room.snapshot();
    let raw = snapshot.encode(false);
    let raw_time = time(3, || {
        std::hint::black_box(snapshot.encode(false));
    });
    let packed = snapshot.encode(true);
    let packed_time = time(3, || {
        std::hint::black_box(snapshot.encode(true));
    });

    let restore_start = Instant::now();
    let decoded = Snapshot::decode(&packed).expect("save decodes");
    let decode_time = restore_start.elapsed();

    let mut fresh = room_for(data.clone(), RoomOptions::default());
    let restore_start = Instant::now();
    fresh.restore(decoded).expect("save restores");
    let restore_time = restore_start.elapsed();

    eprintln!(
        "\n{} slots, {checks} checks, {items} queued items\n\
         snapshot:  {empty_snapshot:?} empty -> {full_snapshot:?} full\n\
         encode:    {:.1} MiB in {raw_time:?} raw, {:.1} MiB in {packed_time:?} deflated \
         ({:.1}x)\n\
         restore:   {decode_time:?} decode + {restore_time:?} install\n",
        slots.len(),
        raw.len() as f64 / (1 << 20) as f64,
        packed.len() as f64 / (1 << 20) as f64,
        raw.len() as f64 / packed.len() as f64,
    );

    // (3) The kill -9 case: what came back is what went in.
    assert_eq!(
        fresh.snapshot().encode(false),
        raw,
        "a save restored at scale must reproduce itself"
    );

    // Deliberately loose. The claim is "snapshot does not scale with room
    // size", and a factor of ten still says that clearly while leaving room for
    // a loaded machine and an unoptimized test build. A deep clone would be
    // several thousand times the empty case, not ten.
    assert!(
        full_snapshot < empty_snapshot * 10 + std::time::Duration::from_millis(1),
        "snapshot cost grew with the room: {empty_snapshot:?} empty vs {full_snapshot:?} full, \
         which means something bulky is being copied rather than shared"
    );
}

#[test]
fn a_save_in_flight_does_not_stall_the_room() {
    if skip_without(FIXTURE) {
        return;
    }
    // Copy-on-write has a cost, and this is where it lands: the first write to
    // a slot after a snapshot copies that slot. It must be paid per slot
    // touched, not per slot in the room — otherwise a save turns the next check
    // batch into a full-room copy, which is the stall the `Arc`s exist to
    // avoid.
    let data = load(FIXTURE).unwrap();
    let slots: Vec<u32> = data.player_slots().map(|(s, _)| *s).collect();
    let mut room = room_for(data.clone(), RoomOptions::default());

    let batch = |slot: u32, skip: usize| -> Vec<i64> {
        data.locations
            .for_slot(slot)
            .iter()
            .skip(skip)
            .take(20)
            .map(|e| e.location)
            .collect()
    };

    let mut sink = Counter::default();
    for &slot in &slots {
        room.register_location_checks((0, slot), &batch(slot, 0), &mut sink);
    }

    // One batch with no save in flight...
    let free_start = Instant::now();
    room.register_location_checks((0, slots[0]), &batch(slots[0], 20), &mut sink);
    let free = free_start.elapsed();

    // ...and one with a snapshot holding every slot's data.
    let held = room.snapshot();
    let copying_start = Instant::now();
    room.register_location_checks((0, slots[1]), &batch(slots[1], 20), &mut sink);
    let copying = copying_start.elapsed();

    eprintln!("check batch: {free:?} with no save in flight, {copying:?} with one");

    // The held snapshot must still show the pre-batch state — that is what
    // makes it a point-in-time copy rather than a live view.
    let held_count = held
        .location_checks
        .iter()
        .find(|(k, _)| *k == (0, slots[1]))
        .map(|(_, c)| c.len())
        .unwrap_or(0);
    assert_eq!(
        held_count, 20,
        "the snapshot moved under us; it is aliasing live state, not copying on write"
    );
    assert_eq!(room.checked_count((0, slots[1])), 40);

    // Loose for the same reasons as above: the point is that one batch copies
    // one slot's set, not two thousand.
    assert!(
        copying < free * 50 + std::time::Duration::from_millis(5),
        "a check batch cost {copying:?} with a save in flight against {free:?} without, \
         which suggests more than the touched slot is being copied"
    );
}

/// Median of `runs`, so one scheduling hiccup does not decide a measurement.
fn time(runs: usize, mut f: impl FnMut()) -> std::time::Duration {
    let mut samples: Vec<std::time::Duration> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    samples.sort_unstable();
    samples[samples.len() / 2]
}
