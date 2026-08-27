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
fn each_fresh_check_is_journaled_once_with_its_finder_and_receiver() {
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
            "journaled a location nobody checked: {record:?}"
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
fn a_location_checked_again_is_not_journaled_again() {
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
    assert_eq!(before, seen.len(), "the release journaled a duplicate");
}

/// Nothing checked, nothing recorded — including for ids the seed does not have.
#[test]
fn unknown_locations_are_not_journaled() {
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

/// The masking is the whole reason chat can be journaled at all.
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
fn ordinary_chat_is_journaled_verbatim() {
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
fn a_cheated_item_is_journaled_since_no_check_can_account_for_it() {
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
fn a_deathlink_is_journaled_and_an_ordinary_bounce_is_not() {
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
        "an unrelated bounce was journaled: {:?}",
        sink.journal_events
    );
}

// --- the connection lifecycle --------------------------------------------

/// **One record per connection, and only once it is authenticated.**
///
/// A slot is not a connection: a player commonly runs a game client, a text
/// client and a tracker, and an organizer reconstructing "who was in the room"
/// needs all three rather than a single presence bit. The pairing is what makes
/// that reconstructable, so a missing `disconnected` leaves a session that
/// never ends.
#[test]
fn a_connection_is_journaled_when_it_joins_and_when_it_goes() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let conn = pahoa_room::ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    // Nothing yet: the socket is open but has authenticated nothing, and a
    // port scan must not be able to write to somebody's disk.
    assert!(
        sink.journal_events_of("connected").is_empty(),
        "an unauthenticated socket was journaled: {:?}",
        sink.journal_events
    );

    room.handle(conn, connect(&name, &game, 0b111), &mut sink);
    let joined = sink.journal_events_of("connected");
    assert_eq!(joined.len(), 1, "{:?}", sink.journal_events);
    let row = joined[0].as_value();
    assert_eq!(row["slot"], slot);
    assert_eq!(row["team"], 0);
    assert_eq!(row["player"], name.as_str());
    assert_eq!(row["game"], game.as_str());
    assert_eq!(row["tags"][0], "AP");
    assert!(
        row["version"].as_str().is_some_and(|v| v.contains('.')),
        "{row}"
    );

    sink.clear();
    room.on_disconnect(conn, &mut sink);
    let left = sink.journal_events_of("disconnected");
    assert_eq!(left.len(), 1, "{:?}", sink.journal_events);
    assert_eq!(left[0].as_value()["slot"], slot);
    assert_eq!(
        left[0].as_value()["slot_empty"],
        true,
        "the slot's last connection went away, so it is dark now"
    );
}

/// `slot_empty` is about the slot, not the connection.
///
/// Closing one of three clients is ordinary; the slot going dark is the thing
/// somebody asks about later. A reader should not have to replay every join and
/// part from the top of the file to work out which just happened.
#[test]
fn a_slot_is_only_empty_once_its_last_connection_leaves() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let first = join(&mut room, 1, &name, &game, 0b111);
    let second = join(&mut room, 2, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.on_disconnect(first, &mut sink);
    assert_eq!(
        sink.journal_events_of("disconnected")[0].as_value()["slot_empty"],
        false,
        "the slot still has a connection on it"
    );

    sink.clear();
    room.on_disconnect(second, &mut sink);
    assert_eq!(
        sink.journal_events_of("disconnected")[0].as_value()["slot_empty"],
        true
    );
}

/// **A tag change is journaled; a `ConnectUpdate` that changes nothing is not.**
///
/// Trackers send these routinely, so recording the packet rather than the
/// change would bury the file in lines saying a client still wants what it
/// already had. Tags are worth recording when they do move: they decide whether
/// a connection may claim the goal, whether it receives chat, and whether it
/// counts as a game client at all.
#[test]
fn only_a_real_tag_change_reaches_the_journal() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let retag = |tags: &[&str]| {
        ClientPacket::ConnectUpdate(cmd::ConnectUpdate {
            items_handling: None,
            tags: Some(tags.iter().map(|t| t.to_string()).collect()),
        })
    };

    let mut sink = Recorder::default();
    room.handle(conn, retag(&["AP", "DeathLink"]), &mut sink);
    let changed = sink.journal_events_of("tags_changed");
    assert_eq!(changed.len(), 1, "{:?}", sink.journal_events);
    let row = changed[0].as_value();
    assert_eq!(row["slot"], slot);
    assert_eq!(row["from"][0], "AP");
    assert_eq!(row["to"][1], "DeathLink");

    // The same tags again, which is what a tracker actually sends.
    sink.clear();
    room.handle(conn, retag(&["AP", "DeathLink"]), &mut sink);
    assert!(
        sink.journal_events_of("tags_changed").is_empty(),
        "a ConnectUpdate that changed nothing was journaled: {:?}",
        sink.journal_events
    );
}

/// **The goal, which nothing recorded before.**
///
/// It is the transition an organizer is asked to adjudicate, it is
/// irreversible, and it triggers auto-release and auto-collect — so without it
/// the `check` records that follow have no explanation in the file. The other
/// statuses churn as clients come and go and say nothing durable.
#[test]
fn reaching_the_goal_is_journaled_exactly_once() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let status = |s: i64| ClientPacket::StatusUpdate(cmd::StatusUpdate { status: s });

    // Playing first: a status that is not the goal must not produce one.
    let mut sink = Recorder::default();
    room.handle(conn, status(20), &mut sink);
    assert!(
        sink.journal_events_of("goal").is_empty(),
        "an ordinary status change was recorded as a goal: {:?}",
        sink.journal_events
    );

    sink.clear();
    room.handle(conn, status(30), &mut sink);
    let goals = sink.journal_events_of("goal");
    assert_eq!(goals.len(), 1, "{:?}", sink.journal_events);
    assert_eq!(goals[0].as_value()["slot"], slot);
    assert_eq!(goals[0].as_value()["player"], name.as_str());
    assert_eq!(goals[0].as_value()["game"], game.as_str());

    // Goal is irreversible, so a repeat must not write a second line — a
    // history saying somebody finished twice is worse than one saying nothing.
    sink.clear();
    room.handle(conn, status(30), &mut sink);
    assert!(
        sink.journal_events_of("goal").is_empty(),
        "the goal was recorded twice: {:?}",
        sink.journal_events
    );
}

// --- the admin surface ----------------------------------------------------

/// **The admin API used to write nothing at all.**
///
/// Sixteen mutating verbs and no record of any of them: an operator could
/// conjure items, force hints, rename a slot or release a world, and the
/// history showed only consequences with no cause. It was also inconsistent —
/// `!getitem` typed into chat has always been a `cheat` record, so whether an
/// action was recorded depended on which door the operator came through, in
/// the artifact that exists to adjudicate exactly that.
#[test]
fn an_admin_command_is_journaled_with_its_target_and_arguments() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.admin(
        pahoa_room::AdminCommand::Kick {
            slot,
            reason: "afk since tuesday".to_string(),
        },
        &mut sink,
    );

    let admin = sink.journal_events_of("admin");
    assert_eq!(admin.len(), 1, "{:?}", sink.journal_events);
    let row = admin[0].as_value();
    assert_eq!(row["command"], "kick");
    assert_eq!(row["slot"], slot);
    assert_eq!(row["detail"]["reason"], "afk since tuesday");
}

/// The gap that made the two doors disagree: an item granted through the admin
/// API left no trace, while the same grant typed as `!getitem` was recorded.
#[test]
fn an_admin_item_grant_is_recorded_like_the_chat_one() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.admin(
        pahoa_room::AdminCommand::SendMultiple {
            slot,
            item: "Archipelago Tarot".to_string(),
            amount: 3,
        },
        &mut sink,
    );

    let admin = sink.journal_events_of("admin");
    assert_eq!(admin.len(), 1, "{:?}", sink.journal_events);
    assert_eq!(admin[0].as_value()["command"], "send_item");
    assert_eq!(admin[0].as_value()["detail"]["amount"], 3);
    assert_eq!(admin[0].as_value()["detail"]["item"], "Archipelago Tarot");
}

/// Reading the room is not an event. An operator refreshing a status page must
/// not fill the history somebody else has to read.
#[test]
fn a_read_only_admin_command_is_not_journaled() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());

    let mut sink = Recorder::default();
    room.admin(pahoa_room::AdminCommand::Status, &mut sink);
    assert!(
        sink.journal_events_of("admin").is_empty(),
        "a read wrote to the history: {:?}",
        sink.journal_events
    );
}

/// **No admin command may put a secret in the file.**
///
/// The one verb carrying a free-text value is `/option`, and the settable table
/// deliberately holds no password — the path refuses one before it reaches the
/// journal. This pins that, because the record writes the value verbatim and
/// the file outlives the room.
#[test]
fn an_admin_option_change_cannot_write_a_password() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let mut room = room_for(data, RoomOptions::default());

    let mut sink = Recorder::default();
    room.admin(
        pahoa_room::AdminCommand::Option {
            name: "server_password".to_string(),
            value: "topsecret".to_string(),
        },
        &mut sink,
    );

    let written = format!("{:?}", sink.journal_events);
    assert!(
        !written.contains("topsecret"),
        "a password reached the journal: {written}"
    );
}

/// **DeathLink was not the only link convention, only the popular one.**
///
/// The server relays every `Bounce` identically, so singling out one tag was a
/// guess about what matters rather than a property of the protocol. Upstream
/// has three — `DeathLink` in 98 worlds, `TrapLink` in 5, `RingLink` in 4 — and
/// they are the same kind of thing: a discrete, player-affecting, cross-game
/// effect. "Why did I get a trap I never earned" is exactly what an organizer
/// is asked, and it was the one question the history could not answer.
#[test]
fn every_link_convention_is_journaled_not_only_deathlink() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let bounce = |tag: &str, data: serde_json::Value| {
        ClientPacket::Bounce(
            cmd::Bounce {
                games: None,
                slots: Some(vec![slot]),
                tags: Some(vec![tag.to_string()]),
                data,
            },
            serde_json::Map::new(),
        )
    };

    let mut sink = Recorder::default();
    room.handle(
        conn,
        bounce(
            "TrapLink",
            serde_json::json!({"source": "amperketBalala", "trap_name": "Ice Trap"}),
        ),
        &mut sink,
    );
    let traps = sink.journal_events_of("traplink");
    assert_eq!(traps.len(), 1, "{:?}", sink.journal_events);
    assert_eq!(traps[0].as_value()["trap_name"], "Ice Trap");
    assert_eq!(traps[0].as_value()["slot"], slot);

    // **The payload keeps its own type.** RingLink counts rings, so `amount` is
    // a number; flattening every convention's field to a string would make a
    // reader parse it back out.
    sink.clear();
    room.handle(
        conn,
        bounce(
            "RingLink",
            serde_json::json!({"source": 1787157140.5, "amount": -25}),
        ),
        &mut sink,
    );
    let rings = sink.journal_events_of("ringlink");
    assert_eq!(rings.len(), 1, "{:?}", sink.journal_events);
    assert_eq!(rings[0].as_value()["amount"], -25);
    // RingLink puts a client instance id where the others put a player name, so
    // there is no name to record and the field is absent rather than wrong.
    assert!(rings[0].as_value()["source"].is_null(), "{:?}", rings[0]);

    // And the boundary still holds: an arbitrary bounce is unbounded in volume
    // and stays out of the file.
    sink.clear();
    room.handle(
        conn,
        bounce("Tracker", serde_json::json!({"whatever": 1})),
        &mut sink,
    );
    assert!(
        sink.journal_events.is_empty(),
        "an unrelated bounce was journaled: {:?}",
        sink.journal_events
    );
}

/// **`source` is what the client said; `slot` is what the server knows.**
///
/// The link payload's `source` is copied straight out of the bounce, so it is
/// unvalidated and a client can put anybody's name in it. The sender recorded
/// beside it comes from the authenticated connection the packet arrived on.
///
/// A history that kept only the claim would be useless for the one question it
/// exists to answer — an organizer asked "who killed me" needs the room's
/// answer, not the packet's assertion — so this pins that the two are recorded
/// separately and that the authoritative one is not taken from the payload.
#[test]
fn a_link_records_the_authenticated_sender_not_the_clients_claim() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::Bounce(
            cmd::Bounce {
                games: None,
                slots: Some(vec![slot]),
                tags: Some(vec!["DeathLink".to_string()]),
                // A client naming somebody who is not the sender, which nothing
                // in the protocol prevents.
                data: serde_json::json!({"cause": "a falling anvil", "source": "NotThisPlayer"}),
            },
            serde_json::Map::new(),
        ),
        &mut sink,
    );

    let row = sink.journal_events_of("deathlink")[0].as_value().clone();
    assert_eq!(row["source"], "NotThisPlayer", "the claim is kept verbatim");
    assert_eq!(row["slot"], slot, "the sender is the connection's slot");
    assert_eq!(row["team"], 0);
    assert_eq!(
        row["player"],
        name.as_str(),
        "the recorded sender must come from the authenticated connection, not \
         from the payload: {row}"
    );
    assert_ne!(
        row["player"], row["source"],
        "the two fields collapsed into one, so a spoofed source would read as \
         the sender"
    );
}
