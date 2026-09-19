//! The join and leave announcements.
//!
//! These are the most-read strings the server produces (every player watches
//! them scroll past) and until this file existed nothing pinned them. They had
//! drifted from the reference in three separate ways at once: the parenthesized
//! field held the game instead of the team, the verb sat where the tag list
//! belongs, and a departure was announced only when the *last* connection for a
//! slot went away, so a player running a game and a tracker together saw
//! nothing when either one left.
//!
//! `tools/gen-message-vectors.py` covers `json_format_send_event` and hints but
//! never reached `on_client_joined`, which is exactly how the drift went
//! unnoticed. The formats here are transcribed from `MultiServer.py:972-976`
//! and `:1001-1006`.

mod common;

use common::*;
use pahoa_proto::server::{PrintJson, PrintJsonType};
use pahoa_proto::types::Version;
use pahoa_proto::{Arg, ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

/// A player slot as `(slot, name, game)`.
type Player = (u32, String, String);

/// Two distinct player slots, so one can watch the other arrive and leave.
fn two_players(data: &pahoa_multidata::MultiData) -> [Player; 2] {
    let mut slots = data
        .player_slots()
        .map(|(slot, info)| (*slot, info.name.clone(), info.game.clone()));
    let a = slots.next().expect("fixture has a player slot");
    let b = slots.next().expect("fixture has a second player slot");
    [a, b]
}

fn connect_tagged(name: &str, game: &str, tags: &[&str]) -> ClientPacket {
    ClientPacket::Connect(Box::new(cmd::Connect {
        password: Arg::Ok(None),
        game: Arg::Ok(Some(game.to_string())),
        name: Arg::Ok(name.to_string()),
        uuid: serde_json::json!("test-uuid"),
        version: Version::new(0, 6, 8),
        items_handling: Arg::Ok(pahoa_proto::lenient::U8(0b111)),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        slot_data: false,
    }))
}

fn text(m: &PrintJson) -> String {
    m.data
        .iter()
        .filter_map(|p| p.text.as_deref())
        .collect::<String>()
}

fn of_type(sink: &Recorder, conn: ConnId, room: &Room, kind: PrintJsonType) -> Vec<String> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::PrintJSON(m) if m.print_type == Some(kind) => Some(text(m)),
            _ => None,
        })
        .collect()
}

/// An observer already in the room, plus everything needed to add more.
fn room_with_observer() -> Option<(Room, ConnId, [Player; 2])> {
    let data = load(FIXTURE)?;
    let players = two_players(&data);
    let mut room = room_for(data, RoomOptions::default());
    let observer = join(&mut room, 1, &players[0].1, &players[0].2, 0b111);
    Some((room, observer, players))
}

#[test]
fn a_join_reads_exactly_as_the_reference_prints_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];

    let mut sink = Recorder::default();
    let conn = ConnId(2);
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect_tagged(name, game, &["AP"]), &mut sink);

    let joins = of_type(&sink, observer, &room, PrintJsonType::Join);
    assert_eq!(
        joins,
        [format!(
            "{name} (Team #1) playing {game} has joined. Client(0.6.8), ['AP']."
        )]
    );
}

/// The shape Troy observed against the reference, with a real tag list:
/// `MooingYacht3 (Team #1) tracking Yacht Dice Bliss has joined. Client(0.5.1),
/// ['Tracker', 'Axolotl', 'DeathLink'].`
#[test]
fn a_non_game_client_puts_its_verb_before_the_game_and_its_tags_last() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];

    let mut sink = Recorder::default();
    let conn = ConnId(2);
    room.on_connect(conn, &mut sink);
    room.handle(
        conn,
        connect_tagged(name, game, &["Tracker", "Axolotl", "DeathLink"]),
        &mut sink,
    );

    let joins = of_type(&sink, observer, &room, PrintJsonType::Join);
    assert_eq!(
        joins,
        [format!(
            "{name} (Team #1) tracking {game} has joined. Client(0.6.8), \
             ['Tracker', 'Axolotl', 'DeathLink']."
        )]
    );
}

#[test]
fn the_tutorial_line_follows_a_join_and_goes_only_to_the_joiner() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];

    let mut sink = Recorder::default();
    let conn = ConnId(2);
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect_tagged(name, game, &["AP"]), &mut sink);

    let mine = of_type(&sink, conn, &room, PrintJsonType::Tutorial);
    assert_eq!(mine.len(), 1, "the joiner is told about !help");
    assert!(mine[0].contains("!help"), "{}", mine[0]);
    assert!(
        of_type(&sink, observer, &room, PrintJsonType::Tutorial).is_empty(),
        "the tutorial is private"
    );
}

/// The bug that produced no departure messages at all.
///
/// A slot commonly has two connections: a game client and a tracker. The
/// reference broadcasts a `Part` for each one that leaves; pahoa announced only
/// when the slot emptied, so the common case was silent.
#[test]
fn a_departure_is_announced_even_when_the_slot_still_has_connections() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];

    // Two connections on the same slot, as a player with a tracker has.
    let playing = ConnId(2);
    let tracking = ConnId(3);
    for (conn, tags) in [(playing, &["AP"][..]), (tracking, &["Tracker"][..])] {
        let mut sink = Recorder::default();
        room.on_connect(conn, &mut sink);
        room.handle(conn, connect_tagged(name, game, tags), &mut sink);
    }

    let mut sink = Recorder::default();
    room.on_disconnect(tracking, "peer closed", &mut sink);

    let parts = of_type(&sink, observer, &room, PrintJsonType::Part);
    assert_eq!(
        parts,
        [format!(
            "{name} (Team #1) has stopped tracking the game. Client(0.6.8), ['Tracker']."
        )],
        "the other connection remaining must not silence this"
    );
}

#[test]
fn a_game_client_leaving_reads_as_left_rather_than_stopped() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];

    let conn = ConnId(2);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);
    room.handle(conn, connect_tagged(name, game, &["AP"]), &mut sink);

    let mut sink = Recorder::default();
    room.on_disconnect(conn, "peer closed", &mut sink);

    assert_eq!(
        of_type(&sink, observer, &room, PrintJsonType::Part),
        [format!(
            "{name} (Team #1) has left the game. Client(0.6.8), ['AP']."
        )]
    );
}

/// The reference picks the verb by scanning *its* table, not the client's tags,
/// so tag order on the wire does not decide the wording.
#[test]
fn the_verb_follows_the_references_tag_priority_not_the_clients_order() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];

    let mut sink = Recorder::default();
    let conn = ConnId(2);
    room.on_connect(conn, &mut sink);
    room.handle(
        conn,
        connect_tagged(name, game, &["Tracker", "HintGame"]),
        &mut sink,
    );

    let joins = of_type(&sink, observer, &room, PrintJsonType::Join);
    assert!(
        joins[0].contains(&format!("hinting {game}")),
        "HintGame outranks Tracker: {}",
        joins[0]
    );
}

// --- tag changes ---------------------------------------------------------
//
// `ConnectUpdate` is answered with nothing at all, so this announcement is the
// only evidence a player has that their tags took. Without it a client toggling
// `DeathLink` cannot tell a working server from one dropping its packets. The
// report that found this said exactly that: "it never seems to update the tags".

/// Retag an existing connection.
fn retag(tags: &[&str]) -> ClientPacket {
    ClientPacket::ConnectUpdate(cmd::ConnectUpdate {
        items_handling: Arg::Missing,
        tags: Some(tags.iter().map(|t| t.to_string()).collect()),
    })
}

#[test]
fn a_tag_change_reads_exactly_as_the_reference_prints_it() {
    if skip_without(FIXTURE) {
        return;
    }
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];
    let conn = ConnId(2);
    let mut setup = Recorder::default();
    room.on_connect(conn, &mut setup);
    room.handle(conn, connect_tagged(name, game, &["AP"]), &mut setup);

    let mut sink = Recorder::default();
    room.handle(conn, retag(&["AP", "DeathLink"]), &mut sink);

    // `MultiServer.py:2031-2033`, tag lists rendered as Python repr.
    assert_eq!(
        of_type(&sink, observer, &room, PrintJsonType::TagsChanged),
        [format!(
            "{name} (Team #1) has changed tags from ['AP'] to ['AP', 'DeathLink']."
        )]
    );
}

#[test]
fn a_tag_change_carries_the_new_tags_as_fields_too() {
    if skip_without(FIXTURE) {
        return;
    }
    // A client that acts on tag changes reads the fields, not the prose.
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (slot, name, game) = &players[1];
    let conn = ConnId(2);
    let mut setup = Recorder::default();
    room.on_connect(conn, &mut setup);
    room.handle(conn, connect_tagged(name, game, &["AP"]), &mut setup);

    let mut sink = Recorder::default();
    room.handle(conn, retag(&["AP", "DeathLink"]), &mut sink);

    let announced: Vec<&PrintJson> = sink
        .packets_for(observer, &room)
        .into_iter()
        .filter_map(|p| match p {
            ServerPacket::PrintJSON(m) if m.print_type == Some(PrintJsonType::TagsChanged) => {
                Some(m)
            }
            _ => None,
        })
        .collect();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].team, Some(0));
    assert_eq!(announced[0].slot, Some(*slot));
    assert_eq!(
        announced[0].tags.as_deref(),
        Some(["AP".to_string(), "DeathLink".to_string()].as_slice())
    );
}

#[test]
fn reordering_the_same_tags_announces_nothing() {
    if skip_without(FIXTURE) {
        return;
    }
    // `set(old_tags) != set(client.tags)` (`MultiServer.py:2025`). A tracker
    // resending its tag list on a timer is the common case, and a line in every
    // player's chat for it would be pure noise.
    let (mut room, observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];
    let conn = ConnId(2);
    let mut setup = Recorder::default();
    room.on_connect(conn, &mut setup);
    room.handle(
        conn,
        connect_tagged(name, game, &["AP", "DeathLink"]),
        &mut setup,
    );

    let mut sink = Recorder::default();
    room.handle(conn, retag(&["DeathLink", "AP"]), &mut sink);

    assert!(
        of_type(&sink, observer, &room, PrintJsonType::TagsChanged).is_empty(),
        "the set is unchanged, so nothing happened"
    );
    assert!(
        sink.journal_events_of("tags_changed").is_empty(),
        "and the journal agrees"
    );

    // The control: a real change still announces, so the comparison above is
    // not simply refusing everything.
    let mut sink = Recorder::default();
    room.handle(conn, retag(&["AP"]), &mut sink);
    assert_eq!(
        of_type(&sink, observer, &room, PrintJsonType::TagsChanged).len(),
        1
    );
}

#[test]
fn a_deathlink_taken_on_after_connecting_receives_bounces() {
    if skip_without(FIXTURE) {
        return;
    }
    // The report this came from: everyone enables DeathLink in-game rather than
    // at connect time, and the bounces never arrived. Tag *state* was applied
    // correctly all along; this pins that, so a future change to the
    // announcement cannot quietly take the routing with it.
    let (mut room, _observer, players) = room_with_observer().unwrap();
    let (_, name, game) = &players[1];
    let conn = ConnId(2);
    let mut setup = Recorder::default();
    room.on_connect(conn, &mut setup);
    room.handle(conn, connect_tagged(name, game, &["AP"]), &mut setup);

    let mut before = Recorder::default();
    room.handle(ConnId(1), deathlink(), &mut before);
    assert!(
        bounces(&before, conn, &room).is_empty(),
        "untagged, so not a recipient"
    );

    let mut sink = Recorder::default();
    room.handle(conn, retag(&["AP", "DeathLink"]), &mut sink);
    let mut after = Recorder::default();
    room.handle(ConnId(1), deathlink(), &mut after);
    assert_eq!(
        bounces(&after, conn, &room).len(),
        1,
        "the new tag should make it a recipient"
    );
}

/// A `Bounce` tagged `DeathLink`, as a client sends one.
fn deathlink() -> ClientPacket {
    let mut raw = serde_json::Map::new();
    raw.insert("cmd".into(), serde_json::json!("Bounce"));
    raw.insert("tags".into(), serde_json::json!(["DeathLink"]));
    raw.insert("data".into(), serde_json::json!({"cause": "a test"}));
    ClientPacket::Bounce(
        cmd::Bounce {
            games: Arg::Missing,
            slots: Arg::Missing,
            tags: Arg::Ok(vec!["DeathLink".to_string()]),
            data: serde_json::json!({"cause": "a test"}),
        },
        raw,
    )
}

fn bounces<'a>(sink: &'a Recorder, conn: ConnId, room: &Room) -> Vec<&'a ServerPacket> {
    sink.packets_for(conn, room)
        .into_iter()
        .filter(|p| matches!(p, ServerPacket::Echo(_)))
        .collect()
}
