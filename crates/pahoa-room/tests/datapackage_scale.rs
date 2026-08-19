//! What a `GetDataPackage` costs the actor.
//!
//! This is the one reply whose size is set by the *seed* rather than by
//! anything a client did, and on a many-game seed it is megabytes. Everything
//! else the room emits is bounded by a slot's location count or a chunk size.
//!
//! The number matters because of where the work happens. The actor owns `Room`
//! and awaits only its mailbox, so any millisecond spent building a reply is a
//! millisecond no other client's packet is processed — and `GetDataPackage`
//! needs no authentication, so any connection can ask for it repeatedly.

mod common;

use common::*;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};
use std::time::Instant;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn encoded_size(sink: &Recorder, conn: ConnId, room: &Room) -> usize {
    sink.packets_for(conn, room)
        .into_iter()
        .filter(|p| matches!(p, ServerPacket::DataPackage(_)))
        .map(|p| pahoa_proto::encode(std::slice::from_ref(p)).len())
        .sum()
}

#[test]
fn a_full_datapackage_reply_stays_off_the_critical_path() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let games = data.games().len();
    let mut room = room_for(data, RoomOptions::default());

    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);

    // Unauthenticated, deliberately: `GetDataPackage` is one of the two packets
    // accepted before `Connect`, so this is reachable by anyone who can open a
    // socket (`MultiServer.py:1963`).
    let mut sink = Recorder::default();
    let started = Instant::now();
    room.handle(
        conn,
        ClientPacket::GetDataPackage(cmd::GetDataPackage {
            games: None,
            exclusions: None,
        }),
        &mut sink,
    );
    let elapsed = started.elapsed();

    // Encoding is on the actor too — `Dispatcher::send` serializes inline —
    // so the honest cost is both phases, not just the handler.
    let encoding = Instant::now();
    let bytes = encoded_size(&sink, conn, &room);
    let encoding = encoding.elapsed();

    eprintln!(
        "GetDataPackage: {games} games, {bytes} bytes, \
         built in {elapsed:?} + encoded in {encoding:?} = {:?} on the actor",
        elapsed + encoding
    );
    assert!(bytes > 0, "a reply should have been produced");

    // The reply is rendered once at construction, so the handler should be a
    // refcount bump — measured at ~4 µs against 4.6 ms when it cloned every
    // name table and serialized them per request. The bound is generous
    // because this runs on whatever CI happens to be; it is set to catch a
    // return to per-request building, not to police microseconds.
    assert!(
        elapsed < std::time::Duration::from_micros(500),
        "the handler took {elapsed:?}, which suggests it is rebuilding the \
         package per request again — the actor is blocked for that long, once \
         per request, for anyone who can open a socket"
    );

    // Encoding is the irreducible part: the bytes have to reach a frame. It is
    // a copy of pre-rendered JSON rather than a serialization of the tables.
    assert!(
        encoding < std::time::Duration::from_millis(5),
        "encoding took {encoding:?}, so the pre-rendered value is not being \
         passed through verbatim"
    );
}

#[test]
fn a_subset_request_costs_only_what_it_asks_for() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let one = {
        let mut names: Vec<String> = data.games().into_iter().collect();
        names.sort();
        names.into_iter().next().expect("fixture has games")
    };
    let mut room = room_for(data, RoomOptions::default());

    let conn = ConnId(1);
    let mut sink = Recorder::default();
    room.on_connect(conn, &mut sink);

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::GetDataPackage(cmd::GetDataPackage {
            games: Some(vec![one.clone()]),
            exclusions: None,
        }),
        &mut sink,
    );

    let subset = encoded_size(&sink, conn, &room);

    let mut sink = Recorder::default();
    room.handle(
        conn,
        ClientPacket::GetDataPackage(cmd::GetDataPackage {
            games: None,
            exclusions: None,
        }),
        &mut sink,
    );
    let full = encoded_size(&sink, conn, &room);

    eprintln!("subset ({one}): {subset} bytes against {full} for everything");
    assert!(
        subset < full,
        "asking for one game should not cost the whole package"
    );
}
