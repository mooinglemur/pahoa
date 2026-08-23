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
fn fresh_room() -> Option<(Room, u32, String, String)> {
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
    let (mut room, ..) = fresh_room().unwrap();
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
    let (mut room, slot, ..) = fresh_room().unwrap();
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
    let (mut room, ..) = fresh_room().unwrap();
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
    let (mut room, ..) = fresh_room().unwrap();
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

/// An announcement must be recognizable *as* one.
///
/// Both halves are trust properties rather than formatting: without the prefix
/// an announcement is bare unattributed text that **impersonates a player**, and
/// with the wrong `type` a client that channels server messages will not treat
/// it as one — `CommandResult` means "the reply to your own command" upstream.
/// The admin API has more than one caller, so neither can be left to them.
#[test]
fn say_is_attributed_to_the_server_and_typed_as_server_chat() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = fresh_room().unwrap();
    let mut sink = Recorder::default();
    room.admin(
        AdminCommand::Say {
            text: "Meow?".into(),
        },
        &mut sink,
    );

    let announced: Vec<&pahoa_proto::server::PrintJson> = sink
        .events
        .iter()
        .filter_map(|e| match e {
            pahoa_room::Event::Broadcast { msgs, .. } => Some(msgs),
            _ => None,
        })
        .flatten()
        .filter_map(|p| match p {
            pahoa_proto::ServerPacket::PrintJSON(m) => Some(m),
            _ => None,
        })
        .collect();

    assert_eq!(announced.len(), 1, "{announced:?}");
    let printed = announced[0];
    assert_eq!(
        printed.print_type,
        Some(pahoa_proto::server::PrintJsonType::ServerChat),
        "an announcement typed as a command reply is not recognizable as one"
    );
    let text: String = printed
        .data
        .iter()
        .filter_map(|p| p.text.as_deref())
        .collect();
    assert_eq!(text, "[Server]: Meow?");
    // The unprefixed original rides along, as upstream sends it, so a client
    // may render either.
    assert_eq!(printed.message.as_deref(), Some("Meow?"));
}

/// The same validator client chat goes through, so an administrator cannot put
/// control characters into every connected client's console.
#[test]
fn say_refuses_what_clients_cannot_render() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = fresh_room().unwrap();
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
    let (mut room, ..) = fresh_room().unwrap();
    assert!(run(&mut room, AdminCommand::Countdown { seconds: 10 }).ok);
    assert!(!run(&mut room, AdminCommand::Countdown { seconds: -1 }).ok);
    assert!(!run(&mut room, AdminCommand::Countdown { seconds: 100_000 }).ok);
}

#[test]
fn send_item_queues_a_real_item_for_the_named_slot() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, name, game) = fresh_room().unwrap();
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
    let (mut room, slot, ..) = fresh_room().unwrap();
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
    let (mut room, slot, name, game) = fresh_room().unwrap();
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
    let (mut room, slot, ..) = fresh_room().unwrap();
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
    let (mut room, slot, ..) = fresh_room().unwrap();
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
    let (mut room, slot, ..) = fresh_room().unwrap();
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

fn say(text: &str) -> pahoa_proto::ClientPacket {
    pahoa_proto::ClientPacket::Say(pahoa_proto::client::Say {
        text: text.to_string(),
    })
}

/// The alias recorded for a slot, read the way the save reads it.
fn alias_of(room: &Room, key: (u32, u32)) -> Option<String> {
    room.snapshot()
        .name_aliases
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// The location name of a slot's first location, if its data package resolved.
fn first_location_name(room: &Room, slot: u32) -> Option<String> {
    let game = &room.multidata().slot_info[&slot].game;
    let id = room.multidata().locations.for_slot(slot).first()?.location;
    let package = room.multidata().embedded_datapackage.get(game)?;
    package
        .location_name_to_id
        .iter()
        .find(|(_, v)| **v == id)
        .map(|(k, _)| k.clone())
}

#[test]
fn hint_location_reaches_the_location_half_of_the_hint_machinery() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let Some(location) = first_location_name(&room, slot) else {
        eprintln!("SKIP: no data package for this slot's game");
        return;
    };

    let outcome = run(
        &mut room,
        AdminCommand::HintLocation {
            slot,
            location: location.clone(),
            force: true,
        },
    );
    assert!(outcome.ok, "{:?}", outcome.output);

    // A *location* hint, not an item one: the hint the slot now holds is for
    // the location that was named.
    let hints = room.hints_for((0, slot));
    assert!(
        hints.iter().any(|h| h.finding_player == slot),
        "hinting a location should produce a hint found in that slot's world"
    );
}

/// The bug this whole command set had: `hint` and `hint_location` differ only
/// by a flag internally, and the admin surface passed `false` unconditionally,
/// so an operator could never reach the location half at all.
#[test]
fn hint_and_hint_location_are_not_the_same_command() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let Some(location) = first_location_name(&room, slot) else {
        eprintln!("SKIP: no data package for this slot's game");
        return;
    };

    // The same string as an *item* name almost certainly matches nothing.
    let as_item = run(
        &mut room,
        AdminCommand::Hint {
            slot,
            item: location.clone(),
            force: true,
        },
    );
    let as_location = run(
        &mut room,
        AdminCommand::HintLocation {
            slot,
            location,
            force: true,
        },
    );
    assert!(
        as_location.ok,
        "the location half should resolve: {:?}",
        as_location.output
    );
    assert!(
        !as_item.ok,
        "a location name resolved as an item — the flag is not being honored"
    );
    assert!(
        as_item.output[0].contains("item"),
        "the refusal should name what it looked for: {:?}",
        as_item.output
    );
}

#[test]
fn send_location_checks_it_for_real() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let key = (0, slot);
    let id = room.multidata().locations.for_slot(slot)[0].location;
    let before = room.checked_count(key);

    let outcome = run(
        &mut room,
        AdminCommand::SendLocation {
            slot,
            location: id.to_string(),
        },
    );
    assert!(outcome.ok, "{:?}", outcome.output);
    assert_eq!(
        room.checked_count(key),
        before + 1,
        "the location should be checked, not merely reported as such"
    );
    assert_eq!(outcome.affected_slots, vec![slot]);
}

/// A second send is a no-op, and says which no-op it was rather than claiming
/// to have checked something again.
#[test]
fn send_location_twice_is_refused_the_second_time() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let id = room.multidata().locations.for_slot(slot)[0].location;
    let command = || AdminCommand::SendLocation {
        slot,
        location: id.to_string(),
    };

    assert!(run(&mut room, command()).ok);
    let second = run(&mut room, command());
    assert!(!second.ok, "{:?}", second.output);
    assert!(
        second.output[0].contains("already checked"),
        "{:?}",
        second.output
    );
}

/// **`allow_release` beats `release_mode`, which is the whole point of it.**
///
/// The reference checks the per-slot exemption first and returns immediately
/// (`MultiServer.py:1511`), so a slot that has been allowed can release under a
/// mode that forbids everyone else.
#[test]
fn allow_release_exempts_one_slot_from_a_forbidding_mode() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            release_mode: pahoa_proto::types::Permission::Disabled,
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    let key = (0, slot);

    // Without the exemption the mode refuses.
    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);
    assert_eq!(room.checked_count(key), 0, "the mode should have refused");

    assert!(
        run(
            &mut room,
            AdminCommand::AllowRelease {
                slot,
                allowed: true
            }
        )
        .ok
    );

    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);
    assert!(
        room.checked_count(key) > 0,
        "an allowed slot must be able to release under a mode that forbids it"
    );
}

/// Clearing the exemption restores the *mode* — it does not forbid releasing,
/// which is why this is one command with a boolean rather than the reference's
/// `forbid_release`, a name that reads like a denial.
#[test]
fn clearing_the_exemption_returns_the_slot_to_the_rooms_mode() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();

    assert!(
        run(
            &mut room,
            AdminCommand::AllowRelease {
                slot,
                allowed: true
            }
        )
        .ok
    );
    let cleared = run(
        &mut room,
        AdminCommand::AllowRelease {
            slot,
            allowed: false,
        },
    );
    assert!(cleared.ok);
    assert!(
        cleared.output[0].contains("release_mode"),
        "the response must say the mode is back in charge, not that releasing is forbidden: {:?}",
        cleared.output
    );
}

#[test]
fn alias_sets_and_clears_another_players_name() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let key = (0, slot);

    assert!(
        run(
            &mut room,
            AdminCommand::Alias {
                slot,
                alias: "Organizer".to_string(),
            },
        )
        .ok
    );
    assert_eq!(alias_of(&room, key).as_deref(), Some("Organizer"));

    assert!(
        run(
            &mut room,
            AdminCommand::Alias {
                slot,
                alias: String::new(),
            },
        )
        .ok
    );
    assert_eq!(alias_of(&room, key), None, "an empty alias should clear it");
}

/// The same 16-character truncation `!alias` applies, so the two surfaces
/// cannot produce names of different lengths.
/// Aliases ride in `NetworkPlayer`, so setting one has to reach *everyone* —
/// the same broadcast `!alias` makes. Without it the operator's change is
/// invisible until each client happens to reconnect.
#[test]
fn an_operator_set_alias_is_pushed_to_every_client() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, name, _) = fresh_room().unwrap();
    let mut sink = Recorder::default();
    room.admin(
        AdminCommand::Alias {
            slot,
            alias: "Organizer".to_string(),
        },
        &mut sink,
    );

    let players = sink
        .events
        .iter()
        .find_map(|e| match e {
            pahoa_room::Event::Broadcast { msgs, .. } => msgs.iter().find_map(|p| match p {
                ServerPacket::RoomUpdate(u) => u.players.as_ref(),
                _ => None,
            }),
            _ => None,
        })
        .expect("everyone gets the new player list");
    let target = players.iter().find(|p| p.slot == slot).unwrap();
    assert_eq!(target.alias, format!("Organizer ({name})"));
    assert_eq!(target.name, name, "the seed name stays visible");
}

#[test]
fn an_operator_set_alias_is_truncated_like_the_chat_one() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    run(
        &mut room,
        AdminCommand::Alias {
            slot,
            alias: "a-very-long-alias-indeed".to_string(),
        },
    );
    assert_eq!(
        alias_of(&room, (0, slot)).as_deref(),
        Some("a-very-long-alia"),
        "the first 16 characters, as the reference takes them"
    );
}

#[test]
fn option_changes_a_rule_over_the_admin_api() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = fresh_room().unwrap();
    let outcome = run(
        &mut room,
        AdminCommand::Option {
            name: "hint_cost".to_string(),
            value: "42".to_string(),
        },
    );
    assert!(outcome.ok, "{:?}", outcome.output);
    assert_eq!(room.options.hint_cost, 42);
}

/// The refusals are the `/option` ones, not a second set written for HTTP —
/// a password cannot be set here for exactly the reason it cannot be set there.
#[test]
fn option_refuses_the_passwords_with_the_same_explanation() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = fresh_room().unwrap();
    let refused = run(
        &mut room,
        AdminCommand::Option {
            name: "server_password".to_string(),
            value: "hunter2".to_string(),
        },
    );
    assert!(!refused.ok);
    assert!(
        refused
            .output
            .iter()
            .any(|l| l.contains("never written to the save")),
        "{:?}",
        refused.output
    );
    assert_eq!(room.options.server_password, None);
}

#[test]
fn option_refuses_an_unknown_name_and_lists_what_it_knows() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, ..) = fresh_room().unwrap();
    let refused = run(
        &mut room,
        AdminCommand::Option {
            name: "nonsense".to_string(),
            value: "1".to_string(),
        },
    );
    assert!(!refused.ok);
    assert!(
        refused.output[0].contains("hint_cost"),
        "{:?}",
        refused.output
    );
}

/// Ids work as well as names, on both hint verbs — the reference accepts them
/// (`MultiServer.py:2443`) and `send_location` already does, so refusing them
/// on one surface would be an inconsistency a caller has to memorize.
#[test]
fn hints_may_be_addressed_by_id() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let id = room.multidata().locations.for_slot(slot)[0].location;

    let outcome = run(
        &mut room,
        AdminCommand::HintLocation {
            slot,
            location: id.to_string(),
            force: true,
        },
    );
    assert!(outcome.ok, "{:?}", outcome.output);
}

/// `send_multiple` queues every copy, on both of the slot's item streams.
#[test]
fn send_multiple_queues_every_copy() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let game = room.multidata().slot_info[&slot].game.clone();
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let outcome = run(
        &mut room,
        AdminCommand::SendMultiple {
            slot,
            item,
            amount: 5,
        },
    );
    assert!(outcome.ok, "{:?}", outcome.output);
    assert!(
        outcome.output[0].contains("5 of"),
        "the count belongs in the response: {:?}",
        outcome.output
    );
    assert_eq!(outcome.affected_slots, vec![slot]);
}

/// One copy reads exactly as `send_item` does, because the reference's `/send`
/// *is* `/send_multiple 1` — the two must not drift into different wording.
#[test]
fn one_copy_reads_the_same_either_way() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let game = room.multidata().slot_info[&slot].game.clone();
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let single = run(
        &mut room,
        AdminCommand::SendItem {
            slot,
            item: item.clone(),
        },
    );
    let multiple_of_one = run(
        &mut room,
        AdminCommand::SendMultiple {
            slot,
            item,
            amount: 1,
        },
    );
    assert_eq!(single.output, multiple_of_one.output);
    assert!(
        !single.output[0].contains(" of "),
        "a single grant must not say \"1 of\": {:?}",
        single.output
    );
}

/// The reference caps this at 100 and so does pahoa: every copy is queued on
/// both streams and replayed from zero on each reconnect, so an accidental
/// extra zero is a room that never finishes sending.
#[test]
fn send_multiple_is_capped() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let game = room.multidata().slot_info[&slot].game.clone();
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let too_many = run(
        &mut room,
        AdminCommand::SendMultiple {
            slot,
            item: item.clone(),
            amount: pahoa_room::SEND_MULTIPLE_LIMIT + 1,
        },
    );
    assert!(!too_many.ok);
    assert!(too_many.output[0].contains("100"), "{:?}", too_many.output);

    // The limit itself is allowed — it is a cap, not a threshold.
    let at_the_limit = run(
        &mut room,
        AdminCommand::SendMultiple {
            slot,
            item: item.clone(),
            amount: pahoa_room::SEND_MULTIPLE_LIMIT,
        },
    );
    assert!(at_the_limit.ok, "{:?}", at_the_limit.output);

    // And nothing below one, which would otherwise grant zero items and
    // cheerfully report success.
    for amount in [0, -1] {
        let refused = run(
            &mut room,
            AdminCommand::SendMultiple {
                slot,
                item: item.clone(),
                amount,
            },
        );
        assert!(!refused.ok, "amount {amount} should be refused");
    }
}

/// The history counts item movements, not commands: five copies is five lines,
/// the same way a multi-location check writes one line per location.
#[test]
fn send_multiple_journals_every_copy() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let game = room.multidata().slot_info[&slot].game.clone();
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let mut sink = Recorder::default();
    room.admin(
        AdminCommand::SendMultiple {
            slot,
            item,
            amount: 3,
        },
        &mut sink,
    );
    let cheats = sink
        .journal_events
        .iter()
        .filter(|e| e.kind() == "cheat")
        .count();
    assert_eq!(
        cheats, 3,
        "three items granted should be three cheat records"
    );
}

/// **The console path announces plainly, and that is upstream's shape, not an
/// oversight on our side.**
///
/// `!getitem` sends a typed `ItemCheat` carrying the `NetworkItem`
/// (`MultiServer.py:1679-1681`); `/send` and `/send_multiple` call
/// `broadcast_text_all` with no additional arguments at all
/// (`MultiServer.py:2389-2392`). A client keying off `type == "ItemCheat"`
/// therefore sees the two differently, so the admin API must not quietly
/// upgrade the console path to the richer form.
#[test]
fn an_admin_grant_is_announced_as_plain_text() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let game = room.multidata().slot_info[&slot].game.clone();
    let Some(item) = first_item_name(&room, &game) else {
        eprintln!("SKIP: no data package for {game}");
        return;
    };

    let mut sink = Recorder::default();
    room.admin(AdminCommand::SendItem { slot, item }, &mut sink);

    let print = sink
        .events
        .iter()
        .find_map(|e| match e {
            pahoa_room::Event::Broadcast { msgs, .. } => msgs.iter().find_map(|p| match p {
                ServerPacket::PrintJSON(print) => Some(print),
                _ => None,
            }),
            _ => None,
        })
        .expect("the grant is announced to the room");

    assert!(
        print.print_type.is_none(),
        "the console path carries no message type: {:?}",
        print.print_type
    );
    assert!(print.item.is_none(), "and no item");
    assert!(print.receiving.is_none(), "and no receiving slot");
    // The text itself is unchanged, so players read the same line either way.
    let text: String = print.data.iter().filter_map(|p| p.text.clone()).collect();
    assert!(text.starts_with("Cheat console: sending "), "{text}");
}

/// **A lock bars the next login and leaves the current session alone.**
///
/// The two halves are separate commands on purpose, and an administrator
/// dealing with a griefer wants both in that order — kicking first leaves a
/// window in which they simply reconnect.
#[test]
fn locking_a_slot_refuses_the_next_connection() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let key = (0, slot);

    // Somebody is already playing.
    let existing = join(&mut room, 1, &name, &game, 0b111);
    assert_eq!(room.connections_for(key), 1);

    let outcome = run(&mut room, AdminCommand::Lock { slot, locked: true });
    assert!(outcome.ok, "{:?}", outcome.output);
    assert!(room.slot_locked(key));

    // The open connection is untouched.
    assert_eq!(
        room.connections_for(key),
        1,
        "locking must not disconnect anyone"
    );
    let mut sink = Recorder::default();
    room.handle(existing, say("hello"), &mut sink);
    assert!(
        !broadcasts(&sink).is_empty(),
        "the connected player should still be able to talk"
    );

    // A new login is refused.
    let refused = join(&mut room, 2, &name, &game, 0b111);
    assert_eq!(
        room.connections_for(key),
        1,
        "a locked slot must not accept a new connection"
    );
    assert!(!room.all_conns().contains(&refused));
}

/// The response says the thing an administrator is most likely to assume
/// wrongly, at the moment they would assume it.
#[test]
fn locking_says_it_did_not_disconnect_anyone() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    join(&mut room, 1, &name, &game, 0b111);

    let outcome = run(&mut room, AdminCommand::Lock { slot, locked: true });
    assert!(
        outcome.output[0].contains("kick"),
        "a lock with someone connected should point at the command that ejects them: {:?}",
        outcome.output
    );

    // With nobody connected there is nothing to disclaim.
    let (mut empty, other, ..) = fresh_room().unwrap();
    let quiet = run(
        &mut empty,
        AdminCommand::Lock {
            slot: other,
            locked: true,
        },
    );
    assert!(!quiet.output[0].contains("kick"), "{:?}", quiet.output);
}

/// **The refusal carries `SlotLocked` beside `InvalidSlot`.**
///
/// `InvalidSlot` is what makes stock clients stop cleanly rather than
/// reconnect-loop (`CommonClient.py:981`); `SlotLocked` is what lets anything
/// reading the raw list tell a lock from a typo. Alone, each fails one of those.
#[test]
fn a_locked_refusal_names_both_reasons() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    room.lock_slot((0, slot), true);

    let conn = ConnId(7);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect(&name, &game, 0b111), &mut sink);

    let refused = sink
        .packets_for(conn, &room)
        .into_iter()
        .find_map(|p| match p {
            ServerPacket::ConnectionRefused(r) => Some(r),
            _ => None,
        })
        .expect("a locked slot is refused");

    assert!(
        refused
            .errors
            .contains(&pahoa_proto::server::ConnectionRefusedReason::SlotLocked),
        "{:?}",
        refused.errors
    );
    assert!(
        refused
            .errors
            .contains(&pahoa_proto::server::ConnectionRefusedReason::InvalidSlot),
        "without this a stock client reconnect-loops forever: {:?}",
        refused.errors
    );
}

/// A lock is not a password mode, so it holds when there is no password at all
/// and it holds against somebody who has the right one.
#[test]
fn a_lock_holds_in_every_password_mode() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);

    for options in [
        RoomOptions::default(),
        RoomOptions {
            password: Some("open-sesame".to_string()),
            ..Default::default()
        },
        RoomOptions {
            slot_passwords: Some(std::collections::BTreeMap::from([(
                slot,
                "open-sesame".to_string(),
            )])),
            ..Default::default()
        },
    ] {
        let mut room = room_for(std::sync::Arc::clone(&data), options);
        room.lock_slot((0, slot), true);

        let conn = ConnId(11);
        let mut sink = Recorder::default();
        room.on_connect(conn, &mut sink);
        room.handle(conn, with_password(&name, &game, "open-sesame"), &mut sink);
        assert_eq!(
            room.connections_for((0, slot)),
            0,
            "the correct password must not open a locked slot"
        );
    }
}

#[test]
fn unlocking_lets_the_slot_back_in() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    run(&mut room, AdminCommand::Lock { slot, locked: true });
    join(&mut room, 1, &name, &game, 0b111);
    assert_eq!(room.connections_for((0, slot)), 0);

    let outcome = run(
        &mut room,
        AdminCommand::Lock {
            slot,
            locked: false,
        },
    );
    assert!(outcome.ok, "{:?}", outcome.output);
    join(&mut room, 2, &name, &game, 0b111);
    assert_eq!(room.connections_for((0, slot)), 1);
}

/// **A lock that a restart lifted would be worse than no lock at all**, since
/// the reason to set one outlives any single process.
#[test]
fn a_lock_survives_a_save_and_restore() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    run(&mut room, AdminCommand::Lock { slot, locked: true });

    let snapshot = room.snapshot();
    let data = load(FIXTURE).unwrap();
    let mut restored = room_for(data, RoomOptions::default());
    restored
        .restore(snapshot)
        .expect("a snapshot this room just produced");

    assert!(
        restored.slot_locked((0, slot)),
        "a restart must not quietly re-admit a locked slot"
    );
}

/// `connect` with a password, for the cases where the password is the point.
fn with_password(name: &str, game: &str, password: &str) -> pahoa_proto::ClientPacket {
    match connect(name, game, 0b111) {
        pahoa_proto::ClientPacket::Connect(mut c) => {
            c.password = Some(password.to_string());
            pahoa_proto::ClientPacket::Connect(c)
        }
        other => other,
    }
}

/// Declaring a goal for a slot does what the slot declaring it would.
///
/// Routed through the same `set_status` a `StatusUpdate` reaches, so the room
/// announces it and the auto rules fire. A bare write to the status map would
/// look identical in every tracker and quietly skip both.
#[test]
fn set_status_goal_announces_and_fires_the_auto_rules() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, ..) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            release_mode: pahoa_proto::types::Permission::AutoEnabled,
            ..Default::default()
        },
    );
    let key = (0, slot);
    assert_eq!(room.status(key), ClientStatus::Unknown);

    let mut sink = Recorder::default();
    let outcome = room.admin(
        AdminCommand::SetStatus {
            slot,
            status: ClientStatus::Goal,
        },
        &mut sink,
    );

    assert!(outcome.ok, "{:?}", outcome.output);
    assert_eq!(room.status(key), ClientStatus::Goal);
    assert!(
        broadcasts(&sink)
            .iter()
            .any(|l| l.contains("has completed their goal")),
        "the room must hear about it: {:?}",
        broadcasts(&sink)
    );
    assert!(
        room.checked_count(key) > 0,
        "release_mode auto should have released their world"
    );
    // And the response warns about that, since a world quietly emptying out is
    // the surprising part.
    assert!(
        outcome.output[0].contains("released"),
        "{:?}",
        outcome.output
    );
}

/// **Goal is a one-way door, from here too.**
///
/// `MultiServer.py:2208` guards every status change with
/// `if current != CLIENT_GOAL`, so not even the client that declared it may
/// take it back. pahoa keeps the invariant rather than carving out an operator
/// exception — but says so rather than ignoring the request, which is what the
/// reference does.
#[test]
fn a_goal_cannot_be_revoked_by_an_administrator() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let goal = |status| AdminCommand::SetStatus { slot, status };

    assert!(run(&mut room, goal(ClientStatus::Goal)).ok);
    assert_eq!(room.status((0, slot)), ClientStatus::Goal);

    let refused = run(&mut room, goal(ClientStatus::Playing));
    assert!(!refused.ok, "{:?}", refused.output);
    assert!(
        refused.output[0].contains("cannot be undone"),
        "the refusal must say why rather than reporting a change that did not happen: {:?}",
        refused.output
    );
    assert_eq!(
        room.status((0, slot)),
        ClientStatus::Goal,
        "the status must be untouched"
    );

    // Even re-declaring the same goal is refused, rather than replaying the
    // announcement and the auto rules a second time.
    assert!(!run(&mut room, goal(ClientStatus::Goal)).ok);
}

#[test]
fn set_status_can_set_the_ordinary_statuses() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    for status in [ClientStatus::Ready, ClientStatus::Playing] {
        let outcome = run(&mut room, AdminCommand::SetStatus { slot, status });
        assert!(outcome.ok, "{:?}", outcome.output);
        assert_eq!(room.status((0, slot)), status);
    }
}

/// `unknown` and `connected` are derived from the connection, so setting one is
/// almost never what somebody meant. Allowed, because a client may send them
/// too, but said out loud.
#[test]
fn setting_a_connection_state_warns_that_it_will_not_stick() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, slot, ..) = fresh_room().unwrap();
    let outcome = run(
        &mut room,
        AdminCommand::SetStatus {
            slot,
            status: ClientStatus::Connected,
        },
    );
    assert!(outcome.ok);
    assert!(
        outcome.output[0].contains("overwritten"),
        "{:?}",
        outcome.output
    );
}
