//! The scoped feed's content filter, at the room level.
//!
//! What is asserted here is the *audience* each message is addressed to, which
//! is what the shards expand. See `docs/scoped-feed.md` for why a scoped
//! connection cannot simply be a `Recipients` variant on the existing sites.

mod common;

use common::*;
use pahoa_proto::client as cmd;
use pahoa_room::{ConnId, Event, FeedPolicy, Recipients, Recorder, Room, RoomOptions, SlotKey};

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

/// Connect a client on a chosen feed policy, discarding the handshake traffic.
fn join_with(room: &mut Room, conn: ConnId, name: &str, game: &str, feed: FeedPolicy) {
    let mut sink = Recorder::default();
    room.on_connect_with_feed(conn, feed, &mut sink);
    room.handle(conn, connect(name, game, 0b111), &mut sink);
}

/// Every audience a run of effects addressed.
fn audiences(sink: &Recorder) -> Vec<Recipients> {
    sink.events
        .iter()
        .filter_map(|e| match e {
            Event::Broadcast { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect()
}

/// The two player slots this fixture starts with, as `(slot, name, game)`.
fn two_players(data: &pahoa_multidata::MultiData) -> [(u32, String, String); 2] {
    let mut players = data
        .player_slots()
        .map(|(slot, info)| (*slot, info.name.clone(), info.game.clone()));
    [
        players.next().expect("a first player"),
        players.next().expect("a second player"),
    ]
}

/// A scoped connection is not derived from tags, and `ConnectUpdate` replaces
/// the tag vector — so this is the test that the policy is sticky.
///
/// Trackers send `ConnectUpdate` routinely to add `DeathLink`. If the policy
/// lived in the tags it would be wiped here, silently, and the connection would
/// fall back to the full firehose with no error anywhere.
#[test]
fn a_connect_update_cannot_lower_the_feed_policy() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let conn = ConnId(1);
    join_with(&mut room, conn, &name, &game, FeedPolicy::Scoped);
    assert!(room.client(conn).unwrap().scoped());

    let mut sink = Recorder::default();
    room.handle(
        conn,
        pahoa_proto::ClientPacket::ConnectUpdate(cmd::ConnectUpdate {
            items_handling: Some(0b111),
            tags: Some(vec!["DeathLink".to_string()]),
        }),
        &mut sink,
    );

    assert!(
        room.client(conn).unwrap().scoped(),
        "a ConnectUpdate must not return this connection to the full feed"
    );
    assert_eq!(room.client(conn).unwrap().tags, vec!["DeathLink"]);
    let _ = slot;
}

/// Chat is never filtered: the feed drops firehose, never anything a human
/// typed.
#[test]
fn chat_and_room_wide_events_stay_addressed_to_everyone() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());
    let conn = ConnId(1);
    join_with(&mut room, conn, &name, &game, FeedPolicy::Scoped);

    let mut sink = Recorder::default();
    room.handle(
        conn,
        pahoa_proto::ClientPacket::Say(cmd::Say {
            text: "hello everyone".to_string(),
        }),
        &mut sink,
    );
    assert!(
        audiences(&sink).contains(&Recipients::AllText),
        "chat must reach every text client, scoped or not: {:?}",
        audiences(&sink)
    );

    // A countdown is room-wide too.
    let mut sink = Recorder::default();
    room.admin(
        pahoa_room::AdminCommand::Countdown { seconds: 3 },
        &mut sink,
    );
    assert!(
        audiences(&sink).contains(&Recipients::AllText),
        "a countdown is room-wide: {:?}",
        audiences(&sink)
    );

    // As is a release announcement.
    let mut sink = Recorder::default();
    room.release_player((0, slot), &mut sink);
    assert!(
        audiences(&sink).contains(&Recipients::AllText),
        "a release announcement is room-wide: {:?}",
        audiences(&sink)
    );
}

/// Joins and parts are attributable to a slot, so a scoped feed hears about its
/// own and not about the other two thousand.
#[test]
fn joins_are_addressed_to_the_slot_they_are_about() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data, RoomOptions::default());

    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect_with_feed(conn, FeedPolicy::Full, &mut sink);
    sink.clear();
    room.handle(conn, connect(&name, &game, 0b111), &mut sink);

    assert!(
        audiences(&sink).contains(&Recipients::AllTextAbout((0, slot))),
        "a join should be attributed to its slot: {:?}",
        audiences(&sink)
    );
}

/// The firehose: a scoped connection receives the item traffic its own slot
/// takes part in, and the full feed still goes out whole to everyone else.
#[test]
fn item_sends_are_routed_to_the_slots_they_concern() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let [(sender, sender_name, sender_game), _] = two_players(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());

    // One scoped connection, on the sending slot.
    join_with(
        &mut room,
        ConnId(1),
        &sender_name,
        &sender_game,
        FeedPolicy::Scoped,
    );

    // Check everything that slot owns, which is the biggest feed it can produce.
    let locations: Vec<i64> = data
        .locations
        .for_slot(sender)
        .iter()
        .map(|e| e.location)
        .collect();
    let mut sink = Recorder::default();
    room.handle(
        ConnId(1),
        pahoa_proto::ClientPacket::LocationChecks(cmd::LocationChecks { locations }),
        &mut sink,
    );

    let audiences = audiences(&sink);
    assert!(
        audiences.contains(&Recipients::AllTextFull),
        "the full feed must still go out whole: {audiences:?}"
    );
    assert!(
        audiences.contains(&Recipients::SlotScopedText((0, sender))),
        "the sending slot's scoped connection should have been routed to: {audiences:?}"
    );
    // And never the undifferentiated firehose, which would defeat the point.
    assert!(
        !audiences.contains(&Recipients::AllText),
        "the item feed must not be addressed to every text client: {audiences:?}"
    );
}

/// With nobody on the scoped port, the router does no work and addresses
/// nothing to it — the property that keeps this free for ordinary rooms.
#[test]
fn no_scoped_connections_means_no_scoped_traffic() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let (slot, name, game) = first_player(&data);
    let mut room = room_for(data.clone(), RoomOptions::default());
    join_with(&mut room, ConnId(1), &name, &game, FeedPolicy::Full);

    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .map(|e| e.location)
        .collect();
    let mut sink = Recorder::default();
    room.handle(
        ConnId(1),
        pahoa_proto::ClientPacket::LocationChecks(cmd::LocationChecks { locations }),
        &mut sink,
    );

    assert!(
        !audiences(&sink)
            .iter()
            .any(|to| matches!(to, Recipients::SlotScopedText(_))),
        "nothing should be routed to a scoped feed nobody is on"
    );
}

/// Resolution is what the shards mirror, so it is worth pinning directly.
#[test]
fn resolution_separates_the_two_policies() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let [(a_slot, a_name, a_game), (b_slot, b_name, b_game)] = two_players(&data);
    let mut room = room_for(data, RoomOptions::default());

    let full = ConnId(1);
    let scoped = ConnId(2);
    join_with(&mut room, full, &a_name, &a_game, FeedPolicy::Full);
    join_with(&mut room, scoped, &b_name, &b_game, FeedPolicy::Scoped);

    let a: SlotKey = (0, a_slot);
    let b: SlotKey = (0, b_slot);

    // Everyone, regardless of policy.
    assert_eq!(room.resolve(&Recipients::AllText), vec![full, scoped]);
    // Only the full-feed one.
    assert_eq!(room.resolve(&Recipients::AllTextFull), vec![full]);
    // Full-feed clients always; the scoped one only when it is the subject.
    assert_eq!(
        room.resolve(&Recipients::AllTextAbout(b)),
        vec![full, scoped]
    );
    assert_eq!(room.resolve(&Recipients::AllTextAbout(a)), vec![full]);
    // Scoped connections of one slot, and never a full-feed one on that slot —
    // it already had the message from the broadcast.
    assert_eq!(room.resolve(&Recipients::SlotScopedText(b)), vec![scoped]);
    assert!(room.resolve(&Recipients::SlotScopedText(a)).is_empty());
}
