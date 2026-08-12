//! The two chat messages a room emits most must be byte-identical to
//! Archipelago's.
//!
//! Vectors come from `MultiServer.json_format_send_event` and
//! `NetUtils.Hint.as_network_message` through `NetUtils.encode`, via
//! `tools/gen-message-vectors.py`. Comparing encoded bytes rather than parsed
//! values is the point: the ids-not-names choice, the `"class"` tag on the
//! embedded `NetworkItem`, and the per-builder key order inside each message
//! part are all invisible to a semantic comparison and all observable to a
//! client.

use pahoa_multidata::{Hint, HintStatus};
use pahoa_proto::types::NetworkItem;
use pahoa_room::Room;
use serde_json::Value;

const VECTORS: &str = include_str!("message_vectors.jsonl");

fn cases() -> impl Iterator<Item = Value> {
    VECTORS
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("vector is JSON"))
}

fn encode(packet: &pahoa_proto::ServerPacket) -> String {
    serde_json::to_string(packet).expect("packets serialize")
}

#[test]
fn the_vector_file_covers_both_message_kinds() {
    let mut item_sends = 0;
    let mut hints = 0;
    for case in cases() {
        match case["kind"].as_str().unwrap() {
            "item_send" => item_sends += 1,
            "hint" => hints += 1,
            other => panic!("unknown vector kind {other}"),
        }
    }
    assert!(item_sends >= 8, "only {item_sends} item-send vectors");
    // Five statuses x three entrances x found/not x local/remote.
    assert!(hints >= 60, "only {hints} hint vectors");
}

#[test]
fn item_send_messages_are_byte_identical_to_archipelago() {
    let mut checked = 0;
    for case in cases().filter(|c| c["kind"] == "item_send") {
        let item = &case["item"];
        let net = NetworkItem {
            item: item["item"].as_i64().unwrap(),
            location: item["location"].as_i64().unwrap(),
            player: item["player"].as_u64().unwrap() as u32,
            flags: item["flags"].as_u64().unwrap() as u32,
        };
        let receiving = case["receiving"].as_u64().unwrap() as u32;

        assert_eq!(
            encode(&Room::item_send_message(receiving, net)),
            case["encoded"].as_str().unwrap(),
            "item {net:?} to slot {receiving}",
        );
        checked += 1;
    }
    assert!(checked > 0, "no item-send vectors ran");
}

#[test]
fn hint_messages_are_byte_identical_to_archipelago() {
    let mut checked = 0;
    for case in cases().filter(|c| c["kind"] == "hint") {
        let h = &case["hint"];
        let hint = Hint {
            receiving_player: h["receiving_player"].as_u64().unwrap() as u32,
            finding_player: h["finding_player"].as_u64().unwrap() as u32,
            location: h["location"].as_i64().unwrap(),
            item: h["item"].as_i64().unwrap(),
            found: h["found"].as_bool().unwrap(),
            entrance: h["entrance"].as_str().unwrap().to_string(),
            item_flags: h["item_flags"].as_u64().unwrap() as u32,
            status: HintStatus::from_i64(
                h["status"].as_i64().unwrap(),
                &pahoa_multidata::Path::root(),
            )
            .expect("vector status is a known value"),
        };

        assert_eq!(
            encode(&Room::hint_message(&hint)),
            case["encoded"].as_str().unwrap(),
            "hint {hint:?}",
        );
        checked += 1;
    }
    assert!(checked > 0, "no hint vectors ran");
}

#[test]
fn a_self_send_reads_as_found_their_rather_than_sent_to() {
    // The two phrasings differ in part count as well as wording, so a client
    // rendering them positionally would break on a mismatch.
    let net = NetworkItem {
        item: 7,
        location: 55,
        player: 3,
        flags: 0,
    };
    let own = encode(&Room::item_send_message(3, net));
    assert!(own.contains(" found their "), "{own}");
    assert!(!own.contains(" sent "), "{own}");

    let other = encode(&Room::item_send_message(4, net));
    assert!(other.contains(" sent "), "{other}");
    assert!(other.contains(" to "), "{other}");
}
