//! What the room offers the journal.
//!
//! The journal is the organizer's record of a room, so the property that
//! matters is that it says what happened **exactly once**. A duplicate makes a
//! history that disagrees with the room; a miss makes one that is quietly
//! short. Both are invisible until someone reads the file months later, which
//! is why the emission is pinned here rather than left to the writer's tests.

mod common;

use common::*;
use pahoa_room::{Recorder, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

#[test]
fn each_fresh_check_is_journalled_once_with_its_finder_and_receiver() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(6)
        .map(|e| e.location)
        .collect();

    let mut sink = Recorder::default();
    room.register_location_checks((0, slot), &locations, &mut sink);

    assert_eq!(
        sink.journal.len(),
        locations.len(),
        "one record per location checked"
    );
    for record in &sink.journal {
        assert_eq!(record.finder, slot, "the finder is whose world it was in");
        assert!(
            locations.contains(&record.location),
            "journalled a location nobody checked: {record:?}"
        );
        // The receiver is the slot the item belongs to, which is routinely not
        // the finder — that is what makes it a multiworld.
        let entry = data
            .locations
            .get(slot, record.location)
            .expect("the location exists");
        assert_eq!(record.receiver, entry.receiver);
        assert_eq!(record.item, entry.item);
        assert_eq!(record.flags, entry.flags);
    }
}

/// The duplicate-suppression that makes the history trustworthy.
#[test]
fn a_location_checked_again_is_not_journalled_again() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(4)
        .map(|e| e.location)
        .collect();

    let mut sink = Recorder::default();
    room.register_location_checks((0, slot), &locations, &mut sink);
    assert_eq!(sink.journal.len(), 4);

    // A client re-sending its whole list on reconnect is ordinary, not an
    // error, and must not append four more lines to the organizer's history.
    sink.clear();
    room.register_location_checks((0, slot), &locations, &mut sink);
    assert!(
        sink.journal.is_empty(),
        "re-checking wrote {} duplicate records",
        sink.journal.len()
    );
}

/// A release is the burst the journal's threading exists for, so it is worth
/// knowing it produces exactly the record set it should.
#[test]
fn a_release_journals_every_remaining_location_once() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let all: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .map(|e| e.location)
        .collect();

    // Check a few first, so the release covers the *remainder* and the total is
    // still exactly the slot's location count with nothing counted twice.
    let mut sink = Recorder::default();
    room.register_location_checks((0, slot), &all[..3], &mut sink);
    let checked_first = sink.journal.len();

    sink.clear();
    room.release_player((0, slot), &mut sink);

    assert_eq!(
        checked_first + sink.journal.len(),
        all.len(),
        "a release plus the earlier checks should account for every location once"
    );
    let mut seen: Vec<i64> = sink.journal.iter().map(|r| r.location).collect();
    seen.sort_unstable();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "the release journalled a duplicate");
}

/// Nothing checked, nothing recorded — including for ids the seed does not have.
#[test]
fn unknown_locations_are_not_journalled() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    // Clients legitimately send ids for locations this multidata does not
    // contain; the room drops them, and the history must not invent them.
    room.register_location_checks((0, slot), &[-1, 0, i64::MAX], &mut sink);
    assert!(sink.journal.is_empty(), "{:?}", sink.journal);
}
