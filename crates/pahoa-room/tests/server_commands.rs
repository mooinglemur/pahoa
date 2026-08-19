//! `!admin` and the `/` command set it dispatches into.
//!
//! Two things are being verified, and they are quite different in kind.
//!
//! The first is the **session**: who may run a command, and what happens at the
//! edges — a wrong password, a displaced administrator, a connection that goes
//! away. The reference holds a single client object for this, so "logged in" is
//! room state rather than connection state, and every interesting case is about
//! that one slot changing hands.
//!
//! The second is that a `/option` **reaches the clients that already believe
//! something else**. Every option it can set was delivered in `RoomInfo` at
//! connect, so a setter that only mutates the room leaves every connected
//! player holding a stale value with nothing to tell them otherwise. That
//! failure is completely silent — the room is correct, the save is correct, and
//! only the players are wrong — which is why the push shape is tested per
//! option rather than once.

mod common;

use common::*;
use pahoa_proto::server::{PrintJson, PrintJsonType, RoomUpdate};
use pahoa_proto::types::Permission;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";
const SERVER_PASSWORD: &str = "hunter2";

fn say(text: &str) -> ClientPacket {
    ClientPacket::Say(cmd::Say {
        text: text.to_string(),
    })
}

/// A room with remote administration enabled and one player connected.
fn setup() -> Option<(Room, ConnId, u32)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            server_password: Some(SERVER_PASSWORD.to_string()),
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    Some((room, conn, slot))
}

fn printed<'a>(
    sink: &'a Recorder,
    conn: ConnId,
    room: &Room,
    kind: PrintJsonType,
) -> Vec<&'a PrintJson> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::PrintJSON(m) if m.print_type == Some(kind) => Some(m),
            _ => None,
        })
        .collect()
}

fn text(p: &PrintJson) -> String {
    p.data
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect()
}

/// What the `/` command set replied to `conn`.
fn admin_said(sink: &Recorder, conn: ConnId, room: &Room) -> Vec<String> {
    printed(sink, conn, room, PrintJsonType::AdminCommandResult)
        .into_iter()
        .map(text)
        .collect()
}

/// What `!admin` itself replied — login and usage lines, which are ordinary
/// `CommandResult` because they come from the client-side processor.
fn shell_said(sink: &Recorder, conn: ConnId, room: &Room) -> Vec<String> {
    printed(sink, conn, room, PrintJsonType::CommandResult)
        .into_iter()
        .map(text)
        .collect()
}

fn room_updates<'a>(sink: &'a Recorder, conn: ConnId, room: &Room) -> Vec<&'a RoomUpdate> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::RoomUpdate(u) => Some(&**u),
            _ => None,
        })
        .collect()
}

/// Log in, discarding the traffic.
fn login(room: &mut Room, conn: ConnId) {
    let mut sink = Recorder::default();
    room.handle(
        conn,
        say(&format!("!admin login {SERVER_PASSWORD}")),
        &mut sink,
    );
    assert!(
        shell_said(&sink, conn, room)
            .iter()
            .any(|line| line.starts_with("Login successful")),
        "login did not take: {:?}",
        shell_said(&sink, conn, room)
    );
}

// --- the session ---------------------------------------------------------

#[test]
fn the_wrong_password_is_refused_and_grants_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin login wrong"), &mut sink);
    assert_eq!(shell_said(&sink, conn, &room), ["Password incorrect."]);

    // And the refusal really did leave the session closed, rather than merely
    // printing something discouraging.
    sink.clear();
    room.handle(conn, say("!admin /option hint_cost 5"), &mut sink);
    assert_eq!(
        shell_said(&sink, conn, &room),
        ["You must first login using !admin login [password]"]
    );
    assert!(admin_said(&sink, conn, &room).is_empty());
}

#[test]
fn a_command_before_logging_in_does_not_run() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    let before = room.options.hint_cost;

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /option hint_cost 99"), &mut sink);

    assert_eq!(
        room.options.hint_cost, before,
        "an unauthenticated set took"
    );
    assert!(!sink.dirty, "an unauthenticated set marked the room dirty");
}

#[test]
fn logging_out_ends_the_session() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin logout"), &mut sink);
    assert_eq!(
        shell_said(&sink, conn, &room),
        ["Logout successful. You can no longer issue server side commands."]
    );

    sink.clear();
    room.handle(conn, say("!admin /options"), &mut sink);
    assert_eq!(
        shell_said(&sink, conn, &room),
        ["You must first login using !admin login [password]"]
    );
}

#[test]
fn a_second_login_displaces_the_first_and_says_so() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (one, two) = {
        let mut slots = data.player_slots();
        let (_, one) = slots.next().unwrap();
        let (_, two) = slots.next().unwrap();
        (one.clone(), two.clone())
    };
    let mut room = room_for(
        data,
        RoomOptions {
            server_password: Some(SERVER_PASSWORD.to_string()),
            ..Default::default()
        },
    );
    let first = join(&mut room, 1, &one.name, &one.game, 0b111);
    let second = join(&mut room, 2, &two.name, &two.game, 0b111);

    login(&mut room, first);

    let mut sink = Recorder::default();
    room.handle(
        second,
        say(&format!("!admin login {SERVER_PASSWORD}")),
        &mut sink,
    );

    // The reference keeps one `commandprocessor.client` and simply overwrites
    // it, leaving the displaced client to discover the loss by trying something.
    // Telling them is a deliberate addition.
    assert!(
        shell_said(&sink, first, &room)
            .iter()
            .any(|line| line.contains("your session has ended")),
        "{:?}",
        shell_said(&sink, first, &room)
    );

    sink.clear();
    room.handle(first, say("!admin /options"), &mut sink);
    assert_eq!(
        shell_said(&sink, first, &room),
        ["You must first login using !admin login [password]"],
        "the displaced administrator kept the session"
    );
}

#[test]
fn a_disconnect_ends_the_session_rather_than_leaving_it_open() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            server_password: Some(SERVER_PASSWORD.to_string()),
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.on_disconnect(conn, &mut sink);

    // The same id again, which is the case worth being sure about: a session
    // that outlived its connection would be inherited by whoever landed on that
    // `ConnId` next, having supplied no password at all.
    let again = join(&mut room, 1, &name, &game, 0b111);
    sink.clear();
    room.handle(again, say("!admin /options"), &mut sink);
    assert_eq!(
        shell_said(&sink, again, &room),
        ["You must first login using !admin login [password]"]
    );
}

// --- setting options -----------------------------------------------------

#[test]
fn setting_an_option_takes_effect_and_marks_the_room_dirty() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /option hint_cost 42"), &mut sink);

    assert_eq!(room.options.hint_cost, 42);
    assert_eq!(
        admin_said(&sink, conn, &room)[0],
        "Set option hint_cost to 42"
    );
    // Without this the change is live but not durable, and reverts at the next
    // restart — the exact defect that rules out a password setter.
    assert!(sink.dirty, "the set did not mark the room dirty");
}

#[test]
fn a_set_option_survives_a_save_round_trip() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /option hint_cost 7"), &mut sink);
    room.handle(conn, say("!admin /option collect_mode goal"), &mut sink);
    room.handle(conn, say("!admin /option item_cheat off"), &mut sink);

    let bytes = room.snapshot().encode(false);
    let data = load(FIXTURE).unwrap();
    let mut restored = room_for(
        data,
        RoomOptions {
            // Deliberately different from what was set, standing in for a flag
            // the room was restarted with: the save is authoritative for these,
            // so the restored values must win.
            hint_cost: 10,
            server_password: Some(SERVER_PASSWORD.to_string()),
            ..Default::default()
        },
    );
    restored
        .restore(pahoa_room::save::Snapshot::decode(&bytes).expect("decodes"))
        .expect("restores");

    assert_eq!(restored.options.hint_cost, 7);
    assert_eq!(restored.options.collect_mode, Permission::Goal);
    assert!(!restored.options.item_cheat);
    // The counterpart, and the reason the password setters are refused: the
    // secret came from configuration and the save had nothing to say about it.
    assert_eq!(
        restored.options.server_password.as_deref(),
        Some(SERVER_PASSWORD)
    );
}

#[test]
fn a_permission_change_pushes_one_room_wide_update() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /option release_mode enabled"), &mut sink);

    let updates = room_updates(&sink, conn, &room);
    assert_eq!(updates.len(), 1, "expected exactly one RoomUpdate");
    let permissions = updates[0]
        .permissions
        .as_ref()
        .expect("permissions were not pushed, so clients keep the old mode");
    assert_eq!(permissions["release"], Permission::Enabled);
    // The whole map, as `get_permissions` sends it — a client replaces rather
    // than merges, so a partial map would blank the other two.
    assert_eq!(permissions.len(), 3);
    assert!(
        updates[0].hint_points.is_none(),
        "points are unrelated here"
    );
}

#[test]
fn changing_check_points_pushes_each_slot_its_own_recomputed_total() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(
        data.clone(),
        RoomOptions {
            location_check_points: 1,
            server_password: Some(SERVER_PASSWORD.to_string()),
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);
    login(&mut room, conn);

    // Something to recompute from. Points are check-count times the per-check
    // value, so a slot with no checks would report zero either way and the test
    // would pass without the recomputation happening at all.
    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(5)
        .map(|e| e.location)
        .collect();
    let mut sink = Recorder::default();
    room.register_location_checks((0, slot), &locations, &mut sink);

    sink.clear();
    room.handle(
        conn,
        say("!admin /option location_check_points 10"),
        &mut sink,
    );

    let updates = room_updates(&sink, conn, &room);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].location_check_points, Some(10));
    assert_eq!(
        updates[0].hint_points,
        Some(room.slot_points((0, slot))),
        "the pushed total is not this slot's"
    );
    assert_eq!(
        updates[0].hint_points,
        Some(locations.len() as i64 * 10),
        "points were not recomputed against the new value"
    );
}

#[test]
fn options_with_no_client_representation_push_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    // `countdown_mode` is absent from `RoomInfo.permissions` and `item_cheat`
    // is absent from `RoomInfo` entirely, so there is nothing on the client to
    // correct. Pushing anyway would be a `RoomUpdate` a client cannot act on.
    for command in [
        "!admin /option countdown_mode auto",
        "!admin /option item_cheat off",
        "!admin /option compatibility 1",
    ] {
        let mut sink = Recorder::default();
        room.handle(conn, say(command), &mut sink);
        assert!(
            room_updates(&sink, conn, &room).is_empty(),
            "{command} pushed a RoomUpdate"
        );
        assert!(sink.dirty, "{command} did not persist");
    }
}

// --- what a setter refuses -----------------------------------------------

#[test]
fn the_password_setters_are_refused_by_name_and_explain_themselves() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    for option in ["password", "server_password"] {
        let mut sink = Recorder::default();
        room.handle(
            conn,
            say(&format!("!admin /option {option} newsecret")),
            &mut sink,
        );

        let said = admin_said(&sink, conn, &room).join(" ");
        // Not "unrecognized": the option exists and is declined, and saying so
        // is the difference between a decision and a gap.
        assert!(
            said.contains("cannot be set while the room is running"),
            "{option}: {said}"
        );
        assert!(said.contains("revert"), "{option} did not say why: {said}");
        assert!(!sink.dirty, "{option} marked the room dirty");
    }

    assert_eq!(room.options.password, None);
    assert_eq!(
        room.options.server_password.as_deref(),
        Some(SERVER_PASSWORD),
        "the refusal did not actually leave the password alone"
    );
}

#[test]
fn the_password_a_refusal_carries_still_never_reaches_the_room() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(
        conn,
        say("!admin /option server_password verysecret"),
        &mut sink,
    );

    // The masking runs before the refusal, which is the whole reason it must
    // stay for a command that is never implemented.
    let chat = printed(&sink, conn, &room, PrintJsonType::Chat);
    assert_eq!(chat.len(), 1);
    let said = text(chat[0]);
    assert!(!said.contains("verysecret"), "leaked into chat: {said}");
    assert!(said.contains("/option server_password *"), "{said}");
}

#[test]
fn an_unknown_option_lists_what_is_settable() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /option nonsense 1"), &mut sink);

    let said = admin_said(&sink, conn, &room).join(" ");
    assert!(said.starts_with("Unrecognized option 'nonsense'"), "{said}");
    assert!(said.contains("hint_cost: int"), "{said}");
    // The listing is what a setter will accept, so the two refused ones must
    // not appear in it.
    assert!(!said.contains("server_password"), "{said}");
}

#[test]
fn a_mode_only_takes_a_value_that_mode_has() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    // `remaining_mode` has no `auto`, and `countdown_mode` has no `goal`
    // (`MultiServer.py:2527-2537`). Both would otherwise parse: `from_text` is
    // a substring test and answers for any string at all.
    for (command, rejected) in [
        ("!admin /option remaining_mode auto", "auto"),
        ("!admin /option countdown_mode goal", "goal"),
        ("!admin /option release_mode sideways", "sideways"),
    ] {
        let mut sink = Recorder::default();
        room.handle(conn, say(command), &mut sink);
        let said = admin_said(&sink, conn, &room).join(" ");
        assert!(said.starts_with("Unrecognized "), "{command}: {said}");
        assert!(said.contains(rejected), "{command}: {said}");
        assert!(!sink.dirty, "{command} took anyway");
    }
}

#[test]
fn auto_enabled_is_accepted_however_the_room_spelled_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    // pahoa prints the hyphen and the reference's valid set holds the
    // underscore, so rejecting either would mean the room refuses a value it
    // had just displayed.
    for spelling in ["auto_enabled", "auto-enabled"] {
        room.options.release_mode = Permission::Disabled;
        let mut sink = Recorder::default();
        room.handle(
            conn,
            say(&format!("!admin /option release_mode {spelling}")),
            &mut sink,
        );
        assert_eq!(
            room.options.release_mode,
            Permission::AutoEnabled,
            "{spelling} was not accepted"
        );
    }
}

#[test]
fn a_number_that_is_not_one_is_rejected_rather_than_silently_zero() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);
    let before = room.options.hint_cost;

    for bad in ["banana", "-5", "3.5"] {
        let mut sink = Recorder::default();
        room.handle(
            conn,
            say(&format!("!admin /option hint_cost {bad}")),
            &mut sink,
        );
        let said = admin_said(&sink, conn, &room).join(" ");
        assert!(said.contains("whole number"), "{bad}: {said}");
        assert_eq!(room.options.hint_cost, before, "{bad} took");
    }
}

// --- the rest of the shell -----------------------------------------------

#[test]
fn a_line_without_a_slash_is_announced_as_the_server() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin ten minutes to the sync"), &mut sink);

    let server_chat = printed(&sink, conn, &room, PrintJsonType::ServerChat);
    assert_eq!(server_chat.len(), 1, "the announcement did not go out");
    assert_eq!(
        text(server_chat[0]),
        "[Server]: ten minutes to the sync",
        "an organizer's announcement should not read as coming from their slot"
    );
}

#[test]
fn an_unknown_slash_command_says_so_rather_than_announcing_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /nonsense"), &mut sink);

    assert!(
        printed(&sink, conn, &room, PrintJsonType::ServerChat).is_empty(),
        "a mistyped command was broadcast to the room"
    );
    assert!(
        admin_said(&sink, conn, &room)[0].starts_with("Unknown command nonsense"),
        "{:?}",
        admin_said(&sink, conn, &room)
    );
}

#[test]
fn the_usage_line_depends_on_whether_you_are_logged_in() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin"), &mut sink);
    assert_eq!(
        shell_said(&sink, conn, &room),
        ["Usage: !admin login [password]"]
    );

    login(&mut room, conn);
    sink.clear();
    room.handle(conn, say("!admin"), &mut sink);
    let said = shell_said(&sink, conn, &room).join(" ");
    assert!(said.contains("!admin /help"), "{said}");
    assert!(said.contains("!admin logout"), "{said}");
}

#[test]
fn options_listing_shows_the_administrator_the_real_server_password() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();
    login(&mut room, conn);

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin /options"), &mut sink);

    // The reference masks this for `!options` and not for `/options`
    // (`MultiServer.py:1412`), and the distinction is sound: whoever is reading
    // this typed that password a moment ago to get here, and the reply goes to
    // that one connection.
    let said = admin_said(&sink, conn, &room).join("\n");
    assert!(
        said.contains(&format!(
            "Option server_password is set to {SERVER_PASSWORD}"
        )),
        "{said}"
    );

    // `!options`, by contrast, still masks it for everyone including this
    // client.
    sink.clear();
    room.handle(conn, say("!options"), &mut sink);
    let public = shell_said(&sink, conn, &room).join("\n");
    assert!(!public.contains(SERVER_PASSWORD), "{public}");
}
