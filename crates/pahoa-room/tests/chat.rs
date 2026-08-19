//! Chat and the `!` command processor.
//!
//! The shape being verified is that every `Say` is chat *first* and a command
//! second — the reference broadcasts the raw line before deciding whether it
//! parses — with `!admin` as the one exception, because a password must never
//! reach the room.

mod common;

use common::*;
use pahoa_proto::server::{PrintJson, PrintJsonType};
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
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

/// The concatenated text of a message's parts.
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

/// The name of the item `slot` is owed in the most places.
///
/// `!hint <name>` resolves the name in the *hinter's own* game and then finds
/// every location holding that item for them, so a test about the one-hint-
/// per-call rule needs an item with more than one placement.
fn most_awaited_item(room: &Room, slot: u32) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for entry in room.multidata().locations.all() {
        if entry.receiver == slot {
            *counts.entry(entry.item).or_default() += 1;
        }
    }
    let game = &room.multidata().slot_info[&slot].game;
    let names = room.datapackage().get(game)?;
    let mut ranked: Vec<(i64, usize)> = counts.into_iter().collect();
    // Deterministic: most placements first, lowest id breaking ties.
    ranked.sort_by_key(|(id, n)| (std::cmp::Reverse(*n), *id));
    ranked
        .into_iter()
        .find_map(|(id, _)| {
            names
                .package
                .item_name_to_id
                .iter()
                .find(|(_, v)| **v == id)
        })
        .map(|(name, _)| name.clone())
}

fn setup() -> Option<(Room, ConnId, u32)> {
    let data = load(FIXTURE)?;
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = join(&mut room, 1, &name, &game, 0b111);
    Some((room, conn, slot))
}

#[test]
fn plain_chat_is_broadcast_with_the_speakers_name() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let name = room.multidata().slot_info[&slot].name.clone();

    let mut sink = Recorder::default();
    room.handle(conn, say("hello everyone"), &mut sink);

    let chat = of_type(&sink, conn, &room, PrintJsonType::Chat);
    assert_eq!(chat.len(), 1);
    assert_eq!(text(chat[0]), format!("{name}: hello everyone"));
    // `message` carries the raw line so clients can re-render it themselves.
    assert_eq!(chat[0].message.as_deref(), Some("hello everyone"));
    assert_eq!(chat[0].slot, Some(slot));
    // Not a command, so nothing else happens.
    assert!(results(&sink, conn, &room).is_empty());
}

#[test]
fn a_command_is_still_said_out_loud() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!players"), &mut sink);

    let chat = of_type(&sink, conn, &room, PrintJsonType::Chat);
    assert_eq!(chat.len(), 1, "the room sees what was typed");
    assert!(text(chat[0]).ends_with(": !players"));
    assert_eq!(results(&sink, conn, &room).len(), 1, "and it ran");
}

#[test]
fn non_printable_text_is_refused() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("two\nlines"), &mut sink);

    let refused: Vec<_> = sink
        .packets_for(conn, &room)
        .into_iter()
        .filter(|p| matches!(p, ServerPacket::InvalidPacket(_)))
        .collect();
    assert_eq!(refused.len(), 1);
    assert!(printed(&sink, conn, &room).is_empty(), "nothing was said");
}

#[test]
fn an_unknown_command_lists_the_known_ones() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!nonsense"), &mut sink);

    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 1);
    assert!(
        out[0].starts_with("Could not find command nonsense."),
        "{out:?}"
    );
    assert!(out[0].contains("hint_location"));
}

#[test]
fn command_names_are_case_insensitive() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!PLAYERS"), &mut sink);
    assert_eq!(results(&sink, conn, &room).len(), 1);
}

#[test]
fn admin_is_never_echoed_verbatim() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!admin login hunter2"), &mut sink);

    let chat = of_type(&sink, conn, &room, PrintJsonType::Chat);
    assert_eq!(chat.len(), 1, "the attempt is announced, masked");
    let said = text(chat[0]);
    assert!(
        !said.contains("hunter2"),
        "password leaked into chat: {said}"
    );
    assert!(said.contains("!admin login *"), "{said}");
    // The mask must not reveal the length either.
    let stars = said.chars().filter(|c| *c == '*').count();
    assert!((4..=16).contains(&stars), "{stars} asterisks");

    assert_eq!(
        results(&sink, conn, &room),
        ["Sorry, Remote administration is disabled"]
    );
}

#[test]
fn setting_an_alias_keeps_the_seed_name_visible() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let name = room.multidata().slot_info[&slot].name.clone();

    let mut sink = Recorder::default();
    room.handle(conn, say("!alias Bob"), &mut sink);

    assert_eq!(results(&sink, conn, &room), ["Hello, Bob"]);
    assert!(sink.dirty);

    // An alias prefixes the seed name rather than replacing it, so other
    // players can still work out who to hint.
    let update = sink
        .packets_for(conn, &room)
        .into_iter()
        .find_map(|p| match p {
            ServerPacket::RoomUpdate(u) => u.players.as_ref(),
            _ => None,
        })
        .expect("everyone gets the new player list");
    let me = update.iter().find(|p| p.slot == slot).unwrap();
    assert_eq!(me.alias, format!("Bob ({name})"));
    assert_eq!(me.name, name);

    // Now it shows up in chat.
    sink.clear();
    room.handle(conn, say("hi"), &mut sink);
    let chat = of_type(&sink, conn, &room, PrintJsonType::Chat);
    assert_eq!(text(chat[0]), format!("Bob ({name}): hi"));

    // And a bare !alias removes it.
    sink.clear();
    room.handle(conn, say("!alias"), &mut sink);
    assert_eq!(results(&sink, conn, &room), ["Removed Alias"]);
}

#[test]
fn an_alias_is_capped_at_sixteen_characters() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!alias 0123456789abcdefGHIJ"), &mut sink);
    assert_eq!(results(&sink, conn, &room), ["Hello, 0123456789abcdef"]);
}

#[test]
fn status_reports_every_slots_progress() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let total = room.multidata().locations.count_for(slot);
    let name = room.multidata().slot_info[&slot].name.clone();

    let mut sink = Recorder::default();
    room.handle(conn, say("!status"), &mut sink);

    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 1);
    assert!(out[0].starts_with("Player Status on team 0:"));
    assert!(
        out[0].contains(&format!("{name} has 1 connection. (0/{total})")),
        "{}",
        &out[0][..400.min(out[0].len())]
    );

    // A tag argument adds the count of connections carrying it.
    sink.clear();
    room.handle(conn, say("!status AP"), &mut sink);
    let tagged = results(&sink, conn, &room);
    assert!(
        tagged[0].contains("1 of which are tagged AP"),
        "{}",
        &tagged[0][..300]
    );
}

#[test]
fn missing_lists_unchecked_locations_and_checked_lists_the_rest() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let location = room.multidata().locations.for_slot(slot)[0].location;
    let total = room.multidata().locations.count_for(slot);

    let mut sink = Recorder::default();
    room.handle(conn, say("!checked"), &mut sink);
    assert_eq!(
        results(&sink, conn, &room),
        ["No done location checks found."]
    );

    sink.clear();
    room.handle(
        conn,
        ClientPacket::LocationChecks(cmd::LocationChecks {
            locations: vec![location],
        }),
        &mut sink,
    );
    sink.clear();

    room.handle(conn, say("!checked"), &mut sink);
    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 2, "one line plus the summary");
    assert!(out[0].starts_with("Checked: "));
    assert_eq!(out[1], "Found 1 done location checks");

    sink.clear();
    room.handle(conn, say("!missing"), &mut sink);
    let out = results(&sink, conn, &room);
    assert_eq!(
        out.last().unwrap(),
        &format!("Found {} missing location checks", total - 1)
    );
}

#[test]
fn a_filter_narrows_the_listing_and_says_so() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let total = room.multidata().locations.count_for(slot);

    // The count of *matches* and the count of missing checks are reported
    // separately, so a filter that matches nothing still says how much there
    // was to filter.
    let mut sink = Recorder::default();
    room.handle(conn, say("!missing zzzzz-no-such-location"), &mut sink);
    let out = results(&sink, conn, &room);
    assert_eq!(
        out,
        [format!(
            "Found {total} missing location checks, displaying 0 of them."
        )]
    );
}

#[test]
fn getitem_cheats_an_item_into_your_inventory() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let game = room.multidata().slot_info[&slot].game.clone();
    // Any item this slot's game defines.
    let item_name = room
        .datapackage()
        .get(&game)
        .unwrap()
        .package
        .item_name_to_id
        .keys()
        .next()
        .cloned()
        .unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say(&format!("!getitem {item_name}")), &mut sink);

    let cheat = of_type(&sink, conn, &room, PrintJsonType::ItemCheat);
    assert_eq!(cheat.len(), 1);
    assert!(text(cheat[0]).contains(&format!("sending \"{item_name}\"")));
    // The cheat sentinel: location -1, sender is the receiving slot itself.
    let item = cheat[0].item.expect("the cheated item rides along");
    assert_eq!(item.location, -1);
    assert_eq!(item.player, slot);

    let got = sink
        .packets_for(conn, &room)
        .into_iter()
        .find_map(|p| match p {
            ServerPacket::ReceivedItems(r) => Some(r),
            _ => None,
        })
        .expect("the item is delivered immediately");
    assert!(got.items.iter().any(|i| i.location == -1));
}

#[test]
fn getitem_answers_a_typo_with_a_suggestion_and_cheats_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    let mut sink = Recorder::default();
    room.handle(conn, say("!getitem zzzzzzzzzzzzzzzz"), &mut sink);

    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("did you mean"), "{out:?}");
    assert!(of_type(&sink, conn, &room, PrintJsonType::ItemCheat).is_empty());
}

#[test]
fn getitem_is_refused_when_cheating_is_off() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(
        data,
        RoomOptions {
            item_cheat: false,
            ..Default::default()
        },
    );
    let conn = join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Recorder::default();
    room.handle(conn, say("!getitem anything"), &mut sink);
    assert_eq!(results(&sink, conn, &room), ["Cheating is disabled."]);
}

#[test]
fn a_bare_hint_quotes_the_price() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let cost = RoomOptions::default().hint_cost_for(room.multidata().locations.count_for(slot));

    let mut sink = Recorder::default();
    room.handle(conn, say("!hint"), &mut sink);

    let out = results(&sink, conn, &room);
    assert_eq!(
        out,
        [format!("A hint costs {cost} points. You have 0 points.")]
    );
}

#[test]
fn hinting_an_item_costs_points_and_grants_exactly_one() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, slot) = setup().unwrap();
    let total = room.multidata().locations.count_for(slot);
    let cost = RoomOptions::default().hint_cost_for(total);

    // Bank enough points to afford one hint but not two.
    let locations: Vec<i64> = room
        .multidata()
        .locations
        .for_slot(slot)
        .iter()
        .map(|e| e.location)
        .take(cost as usize)
        .collect();
    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::LocationChecks(cmd::LocationChecks { locations }),
        &mut sink,
    );
    assert_eq!(room.slot_points((0, slot)), cost);

    // Hint an item this slot receives in several places, so the budget is what
    // limits the result rather than the candidate pool running out.
    let item_name = most_awaited_item(&room, slot).expect("the slot receives some item");
    sink.clear();
    room.handle(conn, say(&format!("!hint {item_name}")), &mut sink);

    let before = room.hints_for((0, slot)).len();
    assert!(before > 0, "a hint was granted");
    assert_eq!(
        room.slot_points((0, slot)),
        0,
        "exactly one hint was charged for"
    );

    // A second attempt cannot afford anything.
    sink.clear();
    room.handle(conn, say(&format!("!hint {item_name}")), &mut sink);
    let out = results(&sink, conn, &room);
    assert!(
        out.iter().any(|t| t.contains("can't afford")
            || t.contains("cannot afford")
            || t.contains("previously used")),
        "{out:?}"
    );
}

#[test]
fn a_non_hintable_name_is_refused_by_name_not_by_id() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, conn, _) = setup().unwrap();

    // Nothing is blacklisted without a data package snapshot, so this checks
    // the shape of the refusal rather than a specific name: an unmatchable
    // string is rejected by the fuzzy matcher first.
    let mut sink = Recorder::default();
    room.handle(conn, say("!hint zzzzzzzzzzzzzzzzzz"), &mut sink);
    let out = results(&sink, conn, &room);
    assert_eq!(out.len(), 1);
    assert!(
        out[0].contains("did you mean") || out[0].contains("Too many close matches"),
        "{out:?}"
    );
}

#[test]
fn no_text_clients_get_no_command_output_at_all() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (_, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let conn = ConnId(9);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    let ClientPacket::Connect(mut c) = connect(&name, &game, 0b111) else {
        unreachable!()
    };
    c.tags = vec!["NoText".to_string()];
    room.handle(conn, ClientPacket::Connect(c), &mut sink);
    sink.clear();

    room.handle(conn, say("!players"), &mut sink);
    assert!(printed(&sink, conn, &room).is_empty());
}
