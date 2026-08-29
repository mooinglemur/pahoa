//! `!release`, `!collect`, `!countdown` and `!remaining`, and the permission
//! modes that gate them.
//!
//! The reference tests those modes two different ways — a substring check for
//! release and collect, equality for remaining and countdown — so `auto-enabled`
//! behaves differently depending on which command you ask. That asymmetry is
//! what most of this file is about.

mod common;

use common::*;
use pahoa_proto::server::{PrintJson, PrintJsonType};
use pahoa_proto::{ClientPacket, ClientStatus, Permission, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn say(text: &str) -> ClientPacket {
    ClientPacket::Say(cmd::Say {
        text: text.to_string(),
    })
}

fn printed<'a>(sink: &'a Recorder, conn: ConnId, room: &Room) -> Vec<&'a PrintJson> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::PrintJSON(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn of_type<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
    kind: PrintJsonType,
) -> Vec<&'a PrintJson> {
    printed(sink, conn, room)
        .into_iter()
        .filter(|m| m.print_type == Some(kind))
        .collect()
}

fn text(m: &PrintJson) -> String {
    m.data
        .iter()
        .filter_map(|p| p.text.as_deref())
        .collect::<String>()
}

fn results(sink: &Recorder, conn: ConnId, room: &Room) -> Vec<String> {
    of_type(sink, conn, room, PrintJsonType::CommandResult)
        .into_iter()
        .map(text)
        .collect()
}

fn room_with(options: RoomOptions) -> Option<(Room, ConnId, u32)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, options);
    let conn = join(&mut room, 1, &name, &game, 0b111);
    Some((room, conn, slot))
}

fn goal(room: &mut Room, conn: ConnId) {
    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::StatusUpdate(cmd::StatusUpdate {
            status: ClientStatus::Goal as i64,
        }),
        &mut sink,
    );
}

fn modes(release: Permission, collect: Permission) -> RoomOptions {
    RoomOptions {
        release_mode: release,
        collect_mode: collect,
        // Keep the goal from triggering an automatic release in tests about
        // the manual path.
        ..Default::default()
    }
}

#[test]
fn releasing_checks_every_location_in_your_world() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) =
        room_with(modes(Permission::Enabled, Permission::Disabled)).unwrap();
    let total = room.multidata().locations.count_for(slot);

    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);

    let announced = of_type(&sink, conn, &room, PrintJsonType::Release);
    assert_eq!(announced.len(), 1);
    assert!(text(announced[0]).contains("has released all remaining items"));

    // Everything is checked, and the slot is told twice: once incrementally by
    // the check registration, once in full afterwards.
    let updates: Vec<_> = sink
        .packets_for(conn, &room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::RoomUpdate(u) => u.checked_locations.as_ref(),
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "incremental then complete");
    assert_eq!(updates[0].len(), total, "everything was new");
    assert_eq!(updates[1].len(), total, "and the full list follows");
    assert!(sink.dirty);
}

#[test]
fn collecting_checks_other_worlds_locations_holding_your_items() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) =
        room_with(modes(Permission::Disabled, Permission::Enabled)).unwrap();

    let owed: usize = room
        .multidata()
        .locations
        .all()
        .iter()
        .filter(|e| e.receiver == slot)
        .count();
    assert!(owed > 0, "the fixture owes this slot something");

    let mut sink = Recorder::default();
    room.handle(conn, say("!collect"), &mut sink);

    let announced = of_type(&sink, conn, &room, PrintJsonType::Collect);
    assert_eq!(announced.len(), 1);
    assert!(text(announced[0]).contains("has collected their items from other worlds"));

    // The items arrive.
    let received: usize = sink
        .packets_for(conn, &room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::ReceivedItems(r) => Some(r.items.len()),
            _ => None,
        })
        .sum();
    assert!(received >= owed, "got {received}, owed {owed}");
}

#[test]
fn a_disabled_mode_refuses_and_points_at_the_admin() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(modes(Permission::Disabled, Permission::Disabled)).unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);
    room.handle(conn, say("!collect"), &mut sink);

    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 2);
    assert!(out[0].contains("item releasing has been disabled"));
    assert!(out[1].contains("collecting has been disabled"));
    assert!(of_type(&sink, conn, &room, PrintJsonType::Release).is_empty());
    assert!(!sink.dirty);
}

#[test]
fn goal_mode_waits_until_the_player_has_finished() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(modes(Permission::Goal, Permission::Goal)).unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);
    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("requires you to have beaten the game"));

    goal(&mut room, conn);
    sink.clear();
    room.handle(conn, say("!release"), &mut sink);
    assert!(
        !of_type(&sink, conn, &room, PrintJsonType::Release).is_empty(),
        "a finished player may release"
    );
}

#[test]
fn auto_enabled_lets_a_player_release_before_finishing() {
    if skip_without(FIXTURE) {
        return;
    }
    // The reference asks `"enabled" in release_mode`, which "auto-enabled"
    // satisfies. The bits in Permission encode exactly that.
    let (mut room, conn, _) =
        room_with(modes(Permission::AutoEnabled, Permission::AutoEnabled)).unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);
    assert!(!of_type(&sink, conn, &room, PrintJsonType::Release).is_empty());
}

#[test]
fn reaching_the_goal_triggers_an_automatic_release_and_collect() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = room_with(modes(Permission::Auto, Permission::Auto)).unwrap();
    let total = room.multidata().locations.count_for(slot);

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::StatusUpdate(cmd::StatusUpdate {
            status: ClientStatus::Goal as i64,
        }),
        &mut sink,
    );

    let kinds: Vec<PrintJsonType> = printed(&sink, conn, &room)
        .into_iter()
        .filter_map(|m| m.print_type)
        .filter(|t| {
            matches!(
                t,
                PrintJsonType::Goal | PrintJsonType::Collect | PrintJsonType::Release
            )
        })
        .collect();
    // Collect runs before release: the finished player's own inventory settles
    // before their world is emptied out.
    assert_eq!(
        kinds,
        [
            PrintJsonType::Goal,
            PrintJsonType::Collect,
            PrintJsonType::Release
        ]
    );
    assert_eq!(room.checked_count((0, slot)), total);
}

#[test]
fn an_administrator_can_grant_one_release_past_the_mode() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) =
        room_with(modes(Permission::Disabled, Permission::Disabled)).unwrap();

    room.allow_release((0, slot), true);
    let mut sink = Recorder::default();
    room.handle(conn, say("!release"), &mut sink);
    assert!(
        !of_type(&sink, conn, &room, PrintJsonType::Release).is_empty(),
        "the grant beats a disabled mode"
    );
}

#[test]
fn remaining_lists_item_names_without_saying_where_they_are() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = room_with(RoomOptions {
        remaining_mode: Permission::Enabled,
        ..Default::default()
    })
    .unwrap();
    let total = room.multidata().locations.count_for(slot);

    let mut sink = Recorder::default();
    room.handle(conn, say("!remaining"), &mut sink);

    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 1);
    let listed = out[0].strip_prefix("Remaining items: ").expect(&out[0]);
    // Counted by separator rather than by name, since item names may contain a
    // comma themselves — this is a floor, not an exact count.
    assert!(listed.split(", ").count() >= total, "{listed}");
    // Nothing about locations or recipients leaks.
    assert!(!listed.contains("AP-"), "{listed}");

    // Once everything is checked there is nothing left to list.
    sink.clear();
    room.release_player((0, slot), pahoa_room::Trigger::Player, &mut sink);
    sink.clear();
    room.handle(conn, say("!remaining"), &mut sink);
    assert_eq!(results(&sink, conn, &room), ["No remaining items found."]);
}

#[test]
fn remaining_treats_auto_enabled_as_goal_gated() {
    if skip_without(FIXTURE) {
        return;
    }
    // Here the reference compares the mode string for *equality*, so
    // "auto-enabled" matches neither "enabled" nor "disabled" and falls through
    // to the goal branch — the opposite of how !release reads the same value.
    let (mut room, conn, _) = room_with(RoomOptions {
        remaining_mode: Permission::AutoEnabled,
        ..Default::default()
    })
    .unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!remaining"), &mut sink);
    assert_eq!(
        results(&sink, conn, &room),
        ["Sorry, !remaining requires you to have beaten the game on this server"]
    );
}

#[test]
fn remaining_can_be_disabled_outright() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(RoomOptions {
        remaining_mode: Permission::Disabled,
        ..Default::default()
    })
    .unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!remaining"), &mut sink);
    assert_eq!(
        results(&sink, conn, &room),
        ["Sorry, !remaining has been disabled on this server."]
    );
}

#[test]
fn a_countdown_announces_every_second_and_then_go() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::Enabled,
        ..Default::default()
    })
    .unwrap();
    let t0 = room.start_time;

    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown 3"), &mut sink);

    // The opening number lands with the announcement, before any waiting.
    let so_far: Vec<String> = of_type(&sink, conn, &room, PrintJsonType::Countdown)
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(
        so_far,
        ["[Server]: Starting countdown of 3s", "[Server]: 3"]
    );

    // Nothing happens before its time.
    sink.clear();
    room.tick(t0 + 0.5, &mut sink);
    assert!(of_type(&sink, conn, &room, PrintJsonType::Countdown).is_empty());

    for (at, expect) in [
        (1.0, "[Server]: 2"),
        (2.0, "[Server]: 1"),
        (3.0, "[Server]: GO"),
    ] {
        sink.clear();
        room.tick(t0 + at, &mut sink);
        let got: Vec<String> = of_type(&sink, conn, &room, PrintJsonType::Countdown)
            .into_iter()
            .map(text)
            .collect();
        assert_eq!(got, [expect], "at t+{at}");
    }
    assert!(room.next_tick().is_none(), "the countdown is over");
}

#[test]
fn a_late_tick_catches_up_rather_than_stretching_the_countdown() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::Enabled,
        ..Default::default()
    })
    .unwrap();
    let t0 = room.start_time;

    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown 3"), &mut sink);
    sink.clear();

    // A stalled thread or a suspended container: one tick, ten seconds late.
    room.tick(t0 + 10.0, &mut sink);
    let got: Vec<String> = of_type(&sink, conn, &room, PrintJsonType::Countdown)
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(got, ["[Server]: 2", "[Server]: 1", "[Server]: GO"]);
    assert!(room.next_tick().is_none());
}

#[test]
fn restarting_a_countdown_retargets_the_running_one() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::Enabled,
        ..Default::default()
    })
    .unwrap();
    let t0 = room.start_time;

    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown 10"), &mut sink);
    sink.clear();

    // A second !countdown does not start a parallel timer; it changes the
    // number the existing one is counting from.
    room.handle(conn, say("!countdown 2"), &mut sink);
    let announced: Vec<String> = of_type(&sink, conn, &room, PrintJsonType::Countdown)
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(
        announced,
        ["[Server]: Starting countdown of 2s"],
        "no second opening number"
    );

    sink.clear();
    room.tick(t0 + 1.0, &mut sink);
    let got: Vec<String> = of_type(&sink, conn, &room, PrintJsonType::Countdown)
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(got, ["[Server]: 2"]);
}

#[test]
fn an_unparseable_countdown_falls_back_to_ten_and_an_hour_is_the_limit() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::Enabled,
        ..Default::default()
    })
    .unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown banana"), &mut sink);
    let got: Vec<String> = of_type(&sink, conn, &room, PrintJsonType::Countdown)
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(got[0], "[Server]: Starting countdown of 10s");

    // Over an hour is refused. The reference prints a Python traceback here;
    // pahoa says what is wrong instead.
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::Enabled,
        ..Default::default()
    })
    .unwrap();
    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown 3601"), &mut sink);
    assert_eq!(
        results(&sink, conn, &room),
        ["3601 is invalid. Maximum is 1 hour."]
    );
    assert!(room.next_tick().is_none());
}

#[test]
fn auto_countdown_turns_itself_off_in_a_large_room() {
    if skip_without(FIXTURE) {
        return;
    }
    // The fixture has 75 slots, well past the reference's threshold of 30.
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::Auto,
        ..Default::default()
    })
    .unwrap();
    assert!(room.multidata().slot_info.len() >= 30);

    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown 3"), &mut sink);
    assert_eq!(
        results(&sink, conn, &room),
        [
            "Sorry, client countdowns have been disabled on this server. \
          You can ask the server admin for a /countdown"
        ]
    );

    // auto-enabled is a different string, so it is not the mode that
    // auto-disables — another place the reference's equality check shows.
    let (mut room, conn, _) = room_with(RoomOptions {
        countdown_mode: Permission::AutoEnabled,
        ..Default::default()
    })
    .unwrap();
    let mut sink = Recorder::default();
    room.handle(conn, say("!countdown 3"), &mut sink);
    assert!(!of_type(&sink, conn, &room, PrintJsonType::Countdown).is_empty());
}
