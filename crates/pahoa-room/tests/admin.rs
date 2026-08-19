//! Administrative commands, driven with no connection behind them.
//!
//! Synchronous, like every other room test: the executor is a `Room` method, so
//! nothing here needs a runtime.

mod common;

use common::*;
use pahoa_multidata::ClientStatus;
use pahoa_proto::ServerPacket;
use pahoa_room::{AdminCommand, AdminOutcome, ConnId, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

/// A room, the first player slot, and its name.
fn room() -> Option<(Room, u32, String, String)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    Some((room_for(data, RoomOptions::default()), slot, name, game))
}

fn run(room: &mut Room, command: AdminCommand) -> AdminOutcome {
    let mut sink = Recorder::default();
    room.admin(command, &mut sink)
}

/// Every line the room broadcast, flattened.
fn broadcasts(sink: &Recorder) -> Vec<String> {
    sink.events
        .iter()
        .filter_map(|e| match e {
            pahoa_room::Event::Broadcast { msgs, .. } => Some(msgs),
            _ => None,
        })
        .flatten()
        .filter_map(|p| match p {
            ServerPacket::PrintJSON(print) => Some(
                print
                    .data
                    .iter()
                    .filter_map(|part| part.text.clone())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

#[test]
fn status_reports_every_slot_without_a_caller() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = room().unwrap();
    let outcome = run(&mut room, AdminCommand::Status);

    assert!(outcome.ok);
    // A summary line, then one per slot.
    assert_eq!(
        outcome.output.len(),
        room.multidata().slot_info.len() + 1,
        "expected a line per slot plus a summary"
    );
    assert!(outcome.output[0].contains("0 of"), "{}", outcome.output[0]);
}

/// The command that motivated the whole design: releasing *someone else's*
/// slot, which no chat command can express.
#[test]
fn release_targets_the_named_slot() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = room().unwrap();
    assert_eq!(room.checked_count((0, slot)), 0);

    let outcome = run(&mut room, AdminCommand::Release { slot });

    assert!(outcome.ok, "{:?}", outcome.output);
    assert_eq!(outcome.affected_slots, vec![slot]);
    assert!(
        room.checked_count((0, slot)) > 0,
        "the slot's locations should have been released"
    );
    assert!(
        outcome.output[0].contains("Released"),
        "{}",
        outcome.output[0]
    );
}

/// An administrator is not bound by the mode that gates players — being able to
/// release for someone who cannot is the point of the API.
#[test]
fn release_ignores_a_mode_that_forbids_players() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, ..) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            release_mode: pahoa_proto::Permission::Disabled,
            ..Default::default()
        },
    );

    let outcome = run(&mut room, AdminCommand::Release { slot });
    assert!(outcome.ok, "{:?}", outcome.output);
    assert!(room.checked_count((0, slot)) > 0);
}

#[test]
fn an_unknown_slot_is_refused_rather_than_ignored() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = room().unwrap();
    for command in [
        AdminCommand::Release { slot: 9999 },
        AdminCommand::Collect { slot: 9999 },
        AdminCommand::Kick {
            slot: 9999,
            reason: String::new(),
        },
        AdminCommand::SendItem {
            slot: 9999,
            item: "Lamp".into(),
        },
        AdminCommand::Hint {
            slot: 9999,
            item: "Lamp".into(),
            force: true,
        },
    ] {
        let outcome = run(&mut room, command.clone());
        assert!(!outcome.ok, "{command:?} should have been refused");
        assert!(outcome.affected_slots.is_empty());
        assert!(outcome.output[0].contains("9999"), "{}", outcome.output[0]);
    }
}

#[test]
fn say_reaches_the_room() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = room().unwrap();
    let mut sink = Recorder::default();
    let outcome = room.admin(
        AdminCommand::Say {
            text: "The async closes on Sunday.".into(),
        },
        &mut sink,
    );

    assert!(outcome.ok);
    assert!(
        broadcasts(&sink)
            .iter()
            .any(|line| line.contains("The async closes on Sunday.")),
        "the message should have been broadcast"
    );
}

/// The same validator client chat goes through, so an administrator cannot put
/// control characters into every connected client's console.
#[test]
fn say_refuses_what_clients_cannot_render() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = room().unwrap();
    let outcome = run(
        &mut room,
        AdminCommand::Say {
            text: "bad\u{7}\u{1b}[2J".into(),
        },
    );
    assert!(!outcome.ok, "{:?}", outcome.output);
}

#[test]
fn countdown_is_bounded() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = room().unwrap();
    assert!(run(&mut room, AdminCommand::Countdown { seconds: 10 }).ok);
    assert!(!run(&mut room, AdminCommand::Countdown { seconds: -1 }).ok);
    assert!(!run(&mut room, AdminCommand::Countdown { seconds: 100_000 }).ok);
}

#[test]
fn send_item_queues_a_real_item_for_the_named_slot() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, name, game) = room().unwrap();
    // A connected client, so the item has somewhere to be delivered.
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect(&name, &game, 0b111), &mut sink);
    sink.clear();

    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let outcome = room.admin(
        AdminCommand::SendItem {
            slot,
            item: item.clone(),
        },
        &mut sink,
    );
    assert!(outcome.ok, "{:?}", outcome.output);
    assert_eq!(outcome.affected_slots, vec![slot]);
    assert!(outcome.output[0].contains(&item), "{}", outcome.output[0]);
    assert!(
        broadcasts(&sink)
            .iter()
            .any(|l| l.contains("Cheat console")),
        "the grant should have been announced as the cheat console does"
    );
}

/// A name matching nothing is refused rather than silently doing nothing, which
/// is the difference from the chat command — there is a caller to answer here.
#[test]
fn send_item_refuses_a_name_that_matches_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = room().unwrap();
    let outcome = run(
        &mut room,
        AdminCommand::SendItem {
            slot,
            item: "definitely not an item".into(),
        },
    );
    assert!(!outcome.ok);
}

/// `item_cheat` gates *players* helping themselves. An administrator granting
/// an item is the sanctioned path that option points people at, so turning it
/// off must not disable the admin route.
#[test]
fn send_item_works_with_the_item_cheat_disabled() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, _, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            item_cheat: false,
            ..Default::default()
        },
    );
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let outcome = run(&mut room, AdminCommand::SendItem { slot, item });
    assert!(outcome.ok, "{:?}", outcome.output);
}

#[test]
fn kick_disconnects_a_connected_slot_and_says_they_may_return() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, name, game) = room().unwrap();
    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect(&name, &game, 0b001), &mut sink);
    sink.clear();

    let outcome = room.admin(
        AdminCommand::Kick {
            slot,
            reason: "the async is over".into(),
        },
        &mut sink,
    );

    assert!(outcome.ok, "{:?}", outcome.output);
    assert_eq!(outcome.affected_slots, vec![slot]);
    assert!(
        outcome.output[0].contains("may reconnect"),
        "a kick is not a ban, and the response should say so: {}",
        outcome.output[0]
    );

    // The reason reaches the client, and the connection is closed.
    assert!(
        broadcasts(&sink)
            .iter()
            .any(|line| line.contains("the async is over")),
        "the reason should have been sent"
    );
    assert!(
        sink.events
            .iter()
            .any(|e| matches!(e, pahoa_room::Event::Close { .. })),
        "the connection should have been closed"
    );
}

#[test]
fn kicking_nobody_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = room().unwrap();
    let outcome = run(
        &mut room,
        AdminCommand::Kick {
            slot,
            reason: String::new(),
        },
    );
    assert!(!outcome.ok);
    assert!(
        outcome.output[0].contains("not connected"),
        "{}",
        outcome.output[0]
    );
}

/// A forced hint bypasses the economy entirely: no points move, because an
/// administrator granting a hint is not the slot buying one.
#[test]
fn a_forced_hint_costs_the_slot_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = room().unwrap();
    let key = (0, slot);
    let before = room.hints_used(key);

    // Any item that slot's game actually has.
    let game = room.multidata().slot_info[&slot].game.clone();
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let outcome = run(
        &mut room,
        AdminCommand::Hint {
            slot,
            item,
            force: true,
        },
    );
    if outcome.ok {
        assert_eq!(
            room.hints_used(key),
            before,
            "a forced hint must not spend the slot's points"
        );
    }
}

/// The status a slot reports is untouched by administration.
#[test]
fn administering_a_slot_does_not_change_its_client_status() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = room().unwrap();
    assert_eq!(room.status((0, slot)), ClientStatus::Unknown);
    run(&mut room, AdminCommand::Release { slot });
    assert_eq!(room.status((0, slot)), ClientStatus::Unknown);
}

/// The first item name the room knows for a game, if its data package resolved.
fn first_item_name(room: &Room, game: &str) -> Option<String> {
    room.multidata()
        .embedded_datapackage
        .get(game)?
        .item_name_to_id
        .keys()
        .next()
        .cloned()
}
