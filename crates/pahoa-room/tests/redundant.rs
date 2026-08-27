//! Requests a client made for work the room had already done.
//!
//! **Nothing here is an error, which is the whole reason it is counted.** A
//! repeated `LocationChecks` is filtered against the slot's existing set and a
//! repeated hint against its hint list, so the room stays correct either way —
//! and a client looping on either is therefore invisible in the log, in the
//! journal, and in any error count, because there is no error. It looks like a
//! busy player.
//!
//! The counters are process-wide statics shared by every test in this binary,
//! so everything here asserts a **delta** around one action.

mod common;

use common::*;
use pahoa_proto::{ClientPacket, client as cmd};
use pahoa_room::redundant::{self, Kind};
use pahoa_room::{Recorder, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

/// A slot re-sending checks it has already made.
///
/// Expected on reconnect — that is how the protocol resynchronizes — and a bug
/// when it happens in a loop. The room cannot tell those apart and does not
/// try; it counts, and the rate against the slot's own traffic is what a reader
/// judges.
#[test]
fn a_location_checked_twice_is_counted_as_redundant() {
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
    let before = redundant::total(Kind::LocationCheck);
    room.register_location_checks((0, slot), &locations, &mut sink);
    assert_eq!(
        redundant::total(Kind::LocationCheck) - before,
        0,
        "the first send of a location is not redundant"
    );

    // The same batch again, which is what a client resending its whole list
    // does.
    let before = redundant::total(Kind::LocationCheck);
    room.register_location_checks((0, slot), &locations, &mut sink);
    assert_eq!(
        redundant::total(Kind::LocationCheck) - before,
        locations.len() as u64,
        "a wholly redundant batch must still be counted, since it is the \
         interesting one"
    );

    // And it is attributed to the slot that sent it, with the kind that
    // distinguishes it from a repeated hint.
    let mine: u64 = redundant::by_slot()
        .into_iter()
        .filter(|((key, kind), _)| *key == (0, slot) && *kind == Kind::LocationCheck)
        .map(|(_, count)| count)
        .sum();
    assert!(mine >= locations.len() as u64, "{mine}");
}

/// **An id this seed does not contain is not redundant, it is unrelated.**
///
/// Clients legitimately send ids for locations a multidata does not have, which
/// the check path has always dropped silently. Counting those would put a
/// permanent floor under every slot of every client that does it and drown the
/// signal this metric exists for.
#[test]
fn an_unknown_location_is_not_counted_as_redundant() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    let before = redundant::total(Kind::LocationCheck);
    // Twice, so that if it were being counted at all the second would land.
    room.register_location_checks((0, slot), &[i64::MAX, i64::MAX - 1], &mut sink);
    room.register_location_checks((0, slot), &[i64::MAX, i64::MAX - 1], &mut sink);
    assert_eq!(
        redundant::total(Kind::LocationCheck) - before,
        0,
        "an id this seed does not contain was counted as a repeat"
    );
}

/// `CreateHints` naming a hint that already exists.
///
/// The room filters it against the slot's hint list and answers correctly, so
/// a client re-scouting in a loop shows up nowhere else.
#[test]
fn a_hint_created_twice_is_counted_as_redundant() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let location = data.locations.for_slot(slot)[0].location;
    let create = || {
        ClientPacket::CreateHints(cmd::CreateHints {
            player: None,
            locations: vec![location],
            status: None,
        })
    };

    let mut sink = Recorder::default();
    let before = redundant::total(Kind::Hint);
    room.handle(conn, create(), &mut sink);
    assert_eq!(
        redundant::total(Kind::Hint) - before,
        0,
        "the first hint for a location is new"
    );

    let before = redundant::total(Kind::Hint);
    room.handle(conn, create(), &mut sink);
    assert_eq!(
        redundant::total(Kind::Hint) - before,
        1,
        "the same hint asked for again was not counted"
    );

    // Attributed to the slot that *asked*, which is the one whose client may be
    // at fault — not to whoever the hinted item belongs to.
    let mine: u64 = redundant::by_slot()
        .into_iter()
        .filter(|((key, kind), _)| *key == (0, slot) && *kind == Kind::Hint)
        .map(|(_, count)| count)
        .sum();
    assert!(mine >= 1, "{mine}");
}

/// A scout that asks for new hints only can detect a repeat; one that
/// re-announces cannot, and must not invent one.
#[test]
fn only_a_scout_asking_for_new_hints_counts_repeats() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let location = data.locations.for_slot(slot)[0].location;
    let scout = |create_as_hint: u8| {
        ClientPacket::LocationScouts(cmd::LocationScouts {
            locations: vec![location],
            create_as_hint: create_as_hint as i64,
        })
    };

    let mut sink = Recorder::default();
    room.handle(conn, scout(2), &mut sink); // banks the hint

    let before = redundant::total(Kind::Hint);
    room.handle(conn, scout(1), &mut sink);
    assert_eq!(
        redundant::total(Kind::Hint) - before,
        0,
        "create_as_hint=1 re-announces rather than filtering, so it cannot \
         observe a repeat and must not report one"
    );

    let before = redundant::total(Kind::Hint);
    room.handle(conn, scout(2), &mut sink);
    assert_eq!(
        redundant::total(Kind::Hint) - before,
        1,
        "a scout asking for new hints only saw an existing one and said nothing"
    );
}
