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

// --- everything that is not a check --------------------------------------

use pahoa_proto::{ClientPacket, client as cmd};

fn say(text: &str) -> ClientPacket {
    ClientPacket::Say(cmd::Say {
        text: text.to_string(),
    })
}

/// The masking is the whole reason chat can be journalled at all.
///
/// A history outlives the room and is handed to a person, so a password
/// reappearing here is worse than one in a log — this is the test that says the
/// masking survives all the way to the file.
#[test]
fn admin_passwords_are_masked_in_the_journal_as_they_are_in_chat() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            server_password: Some("hunter2".to_string()),
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin login hunter2"), &mut sink);
    room.handle(
        conn,
        say("!admin /option server_password topsecret"),
        &mut sink,
    );

    let chat: Vec<String> = sink
        .journal_events_of("chat")
        .iter()
        .map(|e| e.as_value()["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(chat.len(), 2, "{chat:?}");
    for line in &chat {
        assert!(!line.contains("hunter2"), "password in the journal: {line}");
        assert!(
            !line.contains("topsecret"),
            "password in the journal: {line}"
        );
        assert!(line.contains('*'), "not masked at all: {line}");
    }
}

#[test]
fn ordinary_chat_is_journalled_verbatim() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.handle(conn, say("ten minutes to the sync"), &mut sink);

    let chat = sink.journal_events_of("chat");
    assert_eq!(chat.len(), 1);
    let row = chat[0].as_value();
    assert_eq!(row["slot"], slot);
    assert!(
        row["text"]
            .as_str()
            .unwrap()
            .ends_with("ten minutes to the sync"),
        "{row}"
    );
}

#[test]
fn an_option_change_journals_the_change_and_the_resulting_option_set() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            server_password: Some("hunter2".to_string()),
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    let mut sink = Recorder::default();
    room.handle(conn, say("!admin login hunter2"), &mut sink);

    sink.clear();
    room.handle(conn, say("!admin /option collect_mode goal"), &mut sink);

    let changed = sink.journal_events_of("option_changed");
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].as_value()["option"], "collect_mode");
    assert_eq!(changed[0].as_value()["value"], "goal");

    // The full set follows, so a reader never replays every change from the
    // start to learn what the rules were at a moment.
    let options = sink.journal_events_of("options");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].as_value()["collect_mode"], "goal");
    // Modes and flags, never a secret.
    assert_eq!(options[0].as_value()["server_password_set"], true);
    assert!(
        !options[0].as_value().to_string().contains("hunter2"),
        "{}",
        options[0].as_value()
    );
}

#[test]
fn a_cheated_item_is_journalled_since_no_check_can_account_for_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);
    let item = most_owed_item(&room, slot).expect("the seed has a hintable item");

    let mut sink = Recorder::default();
    room.handle(conn, say(&format!("!getitem {item}")), &mut sink);

    let cheats = sink.journal_events_of("cheat");
    assert_eq!(cheats.len(), 1, "{:?}", sink.journal_events);
    let row = cheats[0].as_value();
    assert_eq!(row["slot"], slot);
    assert_eq!(row["item_name"], item.as_str());
    // And no `check` record, because there is no location behind it — which is
    // exactly why this event has to exist.
    assert!(sink.journal.is_empty(), "{:?}", sink.journal);
}

#[test]
fn a_paid_hint_journals_the_balance_either_side() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = richest_player(&data);
    let mut room = room_for(
        data.clone(),
        RoomOptions {
            location_check_points: 50,
            hint_cost: 5,
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);

    // Points to spend. These are the finder's own locations, so the hint below
    // is for something still unfound and therefore chargeable.
    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(8)
        .map(|e| e.location)
        .collect();
    let mut sink = Recorder::default();
    room.register_location_checks((0, slot), &locations, &mut sink);

    let item = most_owed_item(&room, slot).expect("the seed has a hintable item");
    sink.clear();
    room.handle(conn, say(&format!("!hint {item}")), &mut sink);

    let hints = sink.journal_events_of("hints");
    assert_eq!(hints.len(), 1, "{:?}", sink.journal_events);
    let row = hints[0].as_value();
    assert_eq!(row["slot"], slot);
    assert!(
        !row["granted"].as_array().unwrap().is_empty(),
        "a hint event with nothing in it: {row}"
    );
    let before = row["points_before"].as_i64().unwrap();
    let after = row["points_after"].as_i64().unwrap();
    // Both balances rather than the cost alone: hint price is a percentage of a
    // slot's location count and can change mid-room, so a cost in isolation
    // cannot be checked against anything afterwards.
    assert!(
        after < before,
        "a chargeable hint left the balance alone: {before} -> {after}"
    );
    assert_eq!(after, room.slot_points((0, slot)));
}

#[test]
fn a_deathlink_is_journalled_and_an_ordinary_bounce_is_not() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let bounce = |tags: &[&str]| {
        ClientPacket::Bounce(
            cmd::Bounce {
                games: None,
                slots: Some(vec![slot]),
                tags: Some(tags.iter().map(|t| t.to_string()).collect()),
                data: serde_json::json!({"cause": "fell in a pit", "source": "someone"}),
            },
            serde_json::Map::new(),
        )
    };

    let mut sink = Recorder::default();
    room.handle(conn, bounce(&["DeathLink"]), &mut sink);
    let deaths = sink.journal_events_of("deathlink");
    assert_eq!(deaths.len(), 1, "{:?}", sink.journal_events);
    assert_eq!(deaths[0].as_value()["cause"], "fell in a pit");
    assert_eq!(deaths[0].as_value()["slot"], slot);

    // `Bounce` is a general relay that forks and trackers use for their own
    // traffic, and its volume is unbounded in a way checks are not. Only the
    // one an organizer gets asked about is recorded.
    sink.clear();
    room.handle(conn, bounce(&["Tracker"]), &mut sink);
    assert!(
        sink.journal_events_of("deathlink").is_empty(),
        "an unrelated bounce was journalled: {:?}",
        sink.journal_events
    );
}
