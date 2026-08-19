//! M6's exit gate: `!hint` selection against Archipelago's own `get_hints`.
//!
//! Vectors come from `tools/gen-hint-vectors.py`, which drives a real
//! `MultiServer.Context` — only `_load_game_data` is overridden, exactly as
//! `WebHostContext` does it — so `collect_hints`, `get_sphere`,
//! `get_hint_cost` and the payment block are the genuine implementations
//! rather than a second reading of them.
//!
//! # What is compared, and what deliberately is not
//!
//! Hint **ordering** is not comparable and cannot be made so: `get_hints`
//! shuffles a `set`, and `Hint.__hash__` includes the `entrance` string, whose
//! hash CPython randomizes per process. For an entrance-randomized seed the
//! reference does not agree with itself between restarts. So this compares
//! everything that *is* stable:
//!
//! - which hints are announced at all, as a set — the strongest available
//!   check on `collect_hints`, since the free-hint cases announce every
//!   candidate
//! - how many are paid for, and the sphere/locality key each paid hint carries
//! - the points arithmetic before and after
//! - the reply text, word for word

mod common;

use common::*;
use pahoa_proto::server::PrintJsonType;
use pahoa_proto::{ClientPacket, ServerPacket, client as cmd};
use pahoa_room::{Recorder, RoomOptions};
use serde_json::Value;
use std::collections::BTreeSet;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";
const VECTORS: &str = include_str!("hint_vectors.jsonl");

/// A hint reduced to what identifies it on the wire.
type HintKey = (u32, u32, i64, i64, bool);

fn key_of(v: &Value) -> HintKey {
    (
        v["receiving_player"].as_u64().unwrap() as u32,
        v["finding_player"].as_u64().unwrap() as u32,
        v["location"].as_i64().unwrap(),
        v["item"].as_i64().unwrap(),
        v["found"].as_bool().unwrap(),
    )
}

fn cases() -> Vec<Value> {
    VECTORS
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("vector is JSON"))
        .collect()
}

#[test]
fn the_vector_file_covers_the_interesting_branches() {
    let cases = cases();
    assert!(cases.len() >= 8, "only {} vectors", cases.len());
    // Free, priced-and-affordable, priced-and-not, and a rejection.
    assert!(cases.iter().any(|c| c["cost"] == 0));
    assert!(cases.iter().any(|c| c["hints_used"].as_i64() == Some(1)));
    assert!(cases.iter().any(|c| c["hints_used"].as_i64() == Some(0)));
    assert!(cases.iter().any(|c| c["for_location"] == true));
}

#[test]
fn selection_and_pricing_match_archipelago() {
    if skip_without(FIXTURE) {
        return;
    }

    for case in cases() {
        let slot = case["slot"].as_u64().unwrap() as u32;
        let key = (0u32, slot);
        let input = case["input"].as_str().unwrap();
        let for_location = case["for_location"].as_bool().unwrap();
        let label = format!("{input:?} (for_location={for_location})");

        let data = load(FIXTURE).unwrap();
        assert_eq!(
            data.seed_name,
            case["seed_name"].as_str().unwrap(),
            "vectors were generated against a different seed"
        );
        let info = &data.slot_info[&slot];
        let (name, game) = (info.name.clone(), info.game.clone());

        let mut room = room_for(
            data,
            RoomOptions {
                hint_cost: case["hint_cost_percent"].as_u64().unwrap() as u32,
                location_check_points: case["location_check_points"].as_u64().unwrap() as u32,
                ..Default::default()
            },
        );
        let conn = join(&mut room, 1, &name, &game, 0b111);

        // The generator starts each case from an empty hint list, so the
        // seed's precollected hints must not be in play here either.
        room.set_hints(key, Vec::new());

        let mut sink = Recorder::default();
        let checked: Vec<i64> = case["checked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        if !checked.is_empty() {
            room.register_location_checks(key, &checked, &mut sink);
            room.set_hints(key, Vec::new());
        }

        assert_eq!(
            room.slot_points(key),
            case["points_before"].as_i64().unwrap(),
            "points before {label}"
        );
        assert_eq!(
            RoomOptions {
                hint_cost: case["hint_cost_percent"].as_u64().unwrap() as u32,
                ..Default::default()
            }
            .hint_cost_for(room.multidata().locations.count_for(slot)),
            case["cost"].as_i64().unwrap(),
            "hint cost for {label}"
        );

        sink.clear();
        let command = if for_location {
            format!("!hint_location {input}")
        } else {
            format!("!hint {input}")
        };
        room.handle(
            conn,
            ClientPacket::Say(cmd::Say {
                text: command.trim_end().to_string(),
            }),
            &mut sink,
        );

        // --- what was announced -----------------------------------------
        let announced: BTreeSet<HintKey> = sink
            .packets_for(conn, &room)
            .into_iter()
            .filter_map(|p| match p {
                ServerPacket::PrintJSON(m) if m.print_type == Some(PrintJsonType::Hint) => {
                    let item = m.item.expect("a hint message carries its item");
                    Some((
                        m.receiving.expect("and its receiver"),
                        item.player,
                        item.location,
                        item.item,
                        m.found.expect("and whether it is found"),
                    ))
                }
                _ => None,
            })
            .collect();

        let expected: BTreeSet<HintKey> = case["granted"]
            .as_array()
            .unwrap()
            .iter()
            .chain(case["free"].as_array().unwrap())
            .map(key_of)
            .collect();

        // When the budget takes *everything* — free hints, or a pool small
        // enough to exhaust — the two sides must agree exactly, and that is the
        // real check on `collect_hints`: the first vector compares 84 specific
        // placements. When the budget takes a subset, which member of the
        // winning group it takes comes off the shuffle, so only the size can
        // match here; the ordering *rule* is checked separately below.
        let unfound_candidates = case["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| !c["found"].as_bool().unwrap())
            .count();
        let exhaustive = case["granted"].as_array().unwrap().len() == unfound_candidates;

        if exhaustive {
            assert_eq!(
                announced,
                expected,
                "announced hints differ for {label}\n  \
                 pahoa has {} Archipelago does not, and lacks {} it has",
                announced.difference(&expected).count(),
                expected.difference(&announced).count(),
            );
        } else {
            assert_eq!(
                announced.len(),
                expected.len(),
                "announced hint count for {label}"
            );
        }

        // --- what was paid for ------------------------------------------
        assert_eq!(
            room.hints_used(key),
            case["hints_used"].as_i64().unwrap(),
            "hints charged for {label}"
        );
        assert_eq!(
            room.slot_points(key),
            case["points_after"].as_i64().unwrap(),
            "points after {label}"
        );
        // Announced and stored differ on purpose: a found hint is announced
        // but not banked, and neither is one the seed had already placed in
        // the *finding* player's list — `notify_hints` guards on that list, not
        // on the hinting slot's. The first vector exercises both, banking 82 of
        // the 84 it charges for.
        let stored: BTreeSet<HintKey> = room
            .hints_for(key)
            .iter()
            .map(|h| {
                (
                    h.receiving_player,
                    h.finding_player,
                    h.location,
                    h.item,
                    h.found,
                )
            })
            .collect();
        let want_stored: BTreeSet<HintKey> = case["stored"]
            .as_array()
            .unwrap()
            .iter()
            .map(key_of)
            .collect();
        if exhaustive {
            assert_eq!(stored, want_stored, "stored hints for {label}");
        } else {
            // Not comparable: whether the one hint that got paid for was also
            // banked depends on which one the shuffle picked, since the seed
            // had already placed some of these in their finders' lists. It can
            // never exceed what was charged for, though.
            assert!(
                stored.len() <= case["hints_used"].as_i64().unwrap() as usize,
                "banked {} hints having charged for {} on {label}",
                stored.len(),
                case["hints_used"],
            );
        }

        // --- the ordering *rule* ----------------------------------------
        // Which hint wins inside the best group is shuffle-dependent, but the
        // group itself is not: lowest sphere, and a non-local placement ahead
        // of a local one at the same sphere.
        let mut best: Option<(i64, bool)> = None;
        for c in case["candidates"].as_array().unwrap() {
            if c["found"].as_bool().unwrap() {
                continue;
            }
            let k = (c["sphere"].as_i64().unwrap(), c["local"].as_bool().unwrap());
            best = Some(best.map_or(k, |b: (i64, bool)| b.min(k)));
        }
        if let Some(best) = best
            && !exhaustive
        {
            // Read off the announcement rather than the store, since a hint the
            // seed had already banked under its finder is announced without
            // being stored again.
            let free: BTreeSet<HintKey> = case["free"]
                .as_array()
                .unwrap()
                .iter()
                .map(key_of)
                .collect();
            for paid in announced.difference(&free) {
                let candidate = case["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| {
                        c["location"].as_i64() == Some(paid.2)
                            && c["finding_player"].as_u64() == Some(paid.1 as u64)
                    })
                    .unwrap_or_else(|| {
                        panic!("pahoa granted {paid:?}, which is not a candidate, for {label}")
                    });
                let got = (
                    candidate["sphere"].as_i64().unwrap(),
                    candidate["local"].as_bool().unwrap(),
                );
                assert_eq!(
                    got,
                    best,
                    "the paid hint for {label} came from sphere {}, {}; \
                     the rule says sphere {}, {}",
                    got.0,
                    if got.1 { "local" } else { "remote" },
                    best.0,
                    if best.1 { "local" } else { "remote" },
                );
            }
        }

        // --- what the player was told -----------------------------------
        let said: Vec<String> = sink
            .packets_for(conn, &room)
            .into_iter()
            .filter_map(|p| match p {
                ServerPacket::PrintJSON(m)
                    if m.print_type == Some(PrintJsonType::CommandResult) =>
                {
                    Some(
                        m.data
                            .iter()
                            .filter_map(|d| d.text.as_deref())
                            .collect::<String>(),
                    )
                }
                _ => None,
            })
            .collect();
        let want: Vec<String> = case["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

        // When several candidates share the top fuzzy score, *which* one the
        // reference names is not reproducible: `!hint` matches against a
        // `set` (`MultiServer.py:248`), and set iteration order for strings
        // follows `PYTHONHASHSEED`, which CPython randomizes per process. The
        // same seed generated under four hash seeds names four different
        // items — one of which is the one pahoa picks.
        //
        // So the suggestion is elided and everything around it still compared:
        // the score, the wording, and that a rejection happened at all. The
        // same concession this file already makes for hint ordering, and for
        // the same reason.
        if case["suggestion_is_tied"].as_bool().unwrap_or(false) {
            let elide = |lines: &[String]| -> Vec<String> {
                lines
                    .iter()
                    .map(|l| match (l.find('\''), l.rfind('\'')) {
                        (Some(a), Some(b)) if a < b => format!("{}…{}", &l[..=a], &l[b..]),
                        _ => l.clone(),
                    })
                    .collect()
            };
            assert_eq!(
                elide(&said),
                elide(&want),
                "reply text for {label}, ignoring the tied suggestion"
            );
            // The score is the part that must still agree exactly.
            assert!(
                said.iter().zip(&want).all(|(a, b)| {
                    a.rsplit_once('(').map(|(_, s)| s) == b.rsplit_once('(').map(|(_, s)| s)
                }),
                "confidence differs for {label}: {said:?} vs {want:?}"
            );
        } else {
            assert_eq!(said, want, "reply text for {label}");
        }
    }
}

#[test]
fn hint_order_is_reproducible_for_a_given_seed() {
    if skip_without(FIXTURE) {
        return;
    }
    // The half of the exit criterion that *is* about ordering: pahoa's own
    // order must be stable, since the shuffle is seeded from the seed name and
    // insertion order is deterministic. Two identical rooms, same result.
    // Slot and item taken from the vectors rather than written in: the
    // generator picks the richest slot in whatever fixture it was run against,
    // and a hard-coded item name is a fixture that has silently moved on. The
    // first case is the free-hints one, so it grants everything at once.
    let case = cases()
        .into_iter()
        .find(|c| !c["candidates"].as_array().unwrap().is_empty())
        .expect("a vector with candidates to order");
    let subject = case["slot"].as_u64().unwrap() as u32;
    let item = case["input"].as_str().unwrap().to_string();

    let run = || {
        let data = load(FIXTURE).unwrap();
        let info = &data.slot_info[&subject];
        let (name, game) = (info.name.clone(), info.game.clone());
        let mut room = room_for(
            data,
            RoomOptions {
                hint_cost: 0,
                ..Default::default()
            },
        );
        let conn = join(&mut room, 1, &name, &game, 0b111);
        room.set_hints((0, subject), Vec::new());

        let mut sink = Recorder::default();
        room.handle(
            conn,
            ClientPacket::Say(cmd::Say {
                text: format!("!hint {item}"),
            }),
            &mut sink,
        );
        room.hints_for((0, subject))
            .iter()
            .map(|h| (h.finding_player, h.location))
            .collect::<Vec<_>>()
    };

    let first = run();
    assert!(
        first.len() > 1,
        "want several hints to have an order at all"
    );
    assert_eq!(first, run(), "the same seed must give the same order");
}
