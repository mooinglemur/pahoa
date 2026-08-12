//! M3's exit gate: a full release at 2000-slot scale.
//!
//! Two things are being proven, and the second matters more than the first.
//!
//! 1. A ~342k-location release completes quickly enough to run in an ordinary
//!    unit test, which is only possible because this crate has no runtime.
//! 2. It does *not* sweep every connected client. Archipelago's
//!    `send_new_items` iterates all clients on every batch
//!    (`MultiServer.py:1070-1084`) — O(clients) per check, which at 6000
//!    connections is the difference between a room that works and one that
//!    does not.

mod common;

use common::*;
use pahoa_proto::{ClientPacket, client as cmd};
use pahoa_room::{Counter, Recorder, RoomOptions};
use std::time::Instant;

const FIXTURE: &str = "SYNTH_2000slot.archipelago";

#[test]
fn a_full_release_at_scale_is_fast_and_correctly_chunked() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();
    let total_locations = data.locations.len();
    let slots: Vec<u32> = data.player_slots().map(|(s, _)| *s).collect();
    assert!(
        slots.len() > 1000,
        "expected a large fixture, got {} slots",
        slots.len()
    );

    let mut room = room_for(data.clone(), RoomOptions::default());

    // One connection, so the cost measured is the release itself rather than
    // fan-out to thousands of sockets (that belongs to the transport, at M4).
    let (_, name, game) = first_player(&data);
    let _conn = join(&mut room, 1, &name, &game, 0b111);

    let mut sink = Counter::default();
    let started = Instant::now();
    for &slot in &slots {
        let locations: Vec<i64> = data
            .locations
            .for_slot(slot)
            .iter()
            .map(|e| e.location)
            .collect();
        room.register_location_checks((0, slot), &locations, &mut sink);
    }
    let elapsed = started.elapsed();

    // Every location produces one feed line, plus the per-slot RoomUpdate and
    // whatever item deliveries land on the connected client.
    assert!(
        sink.packets >= total_locations,
        "expected at least one packet per location, got {} for {total_locations}",
        sink.packets
    );

    // Chunking must hold: oversized frames would defeat the compression window
    // the 140 figure was chosen for.
    assert!(
        sink.max_chunk <= 140,
        "a broadcast carried {} packets, above the 140 chunk limit",
        sink.max_chunk
    );

    assert!(sink.dirty, "a release must mark the room for saving");

    eprintln!(
        "released {total_locations} locations across {} slots in {:?} \
         ({} broadcasts, {} sends, largest chunk {})",
        slots.len(),
        elapsed,
        sink.broadcasts,
        sink.sends,
        sink.max_chunk,
    );

    // Generous: this is about catching an accidental quadratic, not about
    // benchmarking. A per-check sweep of all clients would blow straight
    // through it.
    assert!(
        elapsed.as_secs() < 60,
        "release took {elapsed:?}, which suggests an accidental O(n^2)"
    );
}

#[test]
fn item_delivery_does_not_sweep_unaffected_clients() {
    if skip_without(FIXTURE) {
        return;
    }
    let data = load(FIXTURE).unwrap();

    // Find a location whose item goes to a slot other than the sender.
    let mut placement = None;
    for (slot, _) in data.player_slots() {
        if let Some(e) = data
            .locations
            .for_slot(*slot)
            .iter()
            .find(|e| e.receiver != *slot)
        {
            placement = Some((*slot, e.location, e.receiver));
            break;
        }
    }
    let Some((sender_slot, location, receiver_slot)) = placement else {
        eprintln!("SKIP: no cross-slot placement in fixture");
        return;
    };

    let mut room = room_for(data.clone(), RoomOptions::default());

    // Connect the sender, the receiver, and a crowd of uninvolved bystanders.
    let info = |s: u32| {
        let i = &data.slot_info[&s];
        (i.name.clone(), i.game.clone())
    };
    let (sn, sg) = info(sender_slot);
    let (rn, rg) = info(receiver_slot);
    let sender = join(&mut room, 1, &sn, &sg, 0b111);
    let receiver = join(&mut room, 2, &rn, &rg, 0b111);

    let mut bystanders = Vec::new();
    for (i, (slot, _)) in data.player_slots().enumerate().take(200) {
        if slot == &sender_slot || slot == &receiver_slot {
            continue;
        }
        let (n, g) = info(*slot);
        bystanders.push(join(&mut room, 100 + i as u64, &n, &g, 0b111));
    }
    assert!(
        bystanders.len() > 100,
        "want a decent crowd to notice a sweep"
    );

    let mut sink = Recorder::default();
    room.handle(
        sender,
        ClientPacket::LocationChecks(cmd::LocationChecks {
            locations: vec![location],
        }),
        &mut sink,
    );

    // The receiver gets items; nobody else does. Bystanders still see the feed
    // line, which is a broadcast, not a per-client sweep.
    let items_to = |c| {
        sink.packets_for(c, &room)
            .into_iter()
            .filter(|p| matches!(p, pahoa_proto::ServerPacket::ReceivedItems(_)))
            .count()
    };

    assert_eq!(items_to(receiver), 1, "receiver should be sent its item");
    assert_eq!(
        items_to(sender),
        0,
        "sender sent the item, it is not for them"
    );
    for b in &bystanders {
        assert_eq!(items_to(*b), 0, "{b} was swept despite receiving nothing");
    }

    // And the direct sends are bounded by who actually received something,
    // rather than by how many clients happen to be connected.
    let sends = sink
        .events
        .iter()
        .filter(|e| matches!(e, pahoa_room::Event::Send { .. }))
        .count();
    assert!(
        sends <= 2,
        "expected at most the receiver's connections, got {sends} sends"
    );
}
