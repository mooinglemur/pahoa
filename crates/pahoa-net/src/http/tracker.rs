//! Rendering `/api/tracker` and `/api/static_tracker`.
//!
//! A faithful mirror of the reference WebHost's endpoints, including the parts
//! that are only the way they are because of how Flask serializes Python — see
//! `docs/tracker.md`. Deviating anywhere makes this a *different* API that
//! merely resembles the reference, which is the one thing it must not be.

use pahoa_room::tracker::TrackerData;
use serde_json::{Value, json};

/// The live document.
pub fn tracker(data: &TrackerData) -> Value {
    // The reference walks two different sets, and the difference is invisible
    // until a seed has a spectator or an item-link group in it:
    // `get_all_players()` — players only — for every per-player array, and
    // `get_all_slots()` for hints alone. A spectator has no progress to report
    // and a group has no client behind it.
    let players: Vec<&pahoa_room::tracker::TrackerSlot> =
        data.slots.iter().filter(|s| s.playing).collect();

    json!({
        "aliases": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot, "alias": s.alias,
        })).collect::<Vec<_>>(),

        "player_items_received": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot,
            "items": s.items_received.iter().map(item).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),

        "player_checks_done": players.iter().map(|s| {
            // Sorted, as the reference sorts them: a tracker diffing two polls
            // should not see a reordering as a change.
            let mut locations: Vec<i64> = s.checks.iter().copied().collect();
            locations.sort_unstable();
            json!({ "team": s.team, "player": s.slot, "locations": locations })
        }).collect::<Vec<_>>(),

        "total_checks_done": data.total_checks.iter().map(|(team, done)| json!({
            "team": team, "checks_done": done,
        })).collect::<Vec<_>>(),

        "hints": data.slots.iter().map(|s| json!({
            "team": s.team, "player": s.slot,
            "hints": s.hints.iter().map(hint).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),

        "activity_timers": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot, "time": rfc1123(s.last_activity),
        })).collect::<Vec<_>>(),

        "connection_timers": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot, "time": rfc1123(s.last_connection),
        })).collect::<Vec<_>>(),

        "player_status": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot, "status": s.status as i64,
        })).collect::<Vec<_>>(),
    })
}

/// The document that only changes when the seed does.
pub fn static_tracker(data: &TrackerData) -> Value {
    let players: Vec<&pahoa_room::tracker::TrackerSlot> =
        data.slots.iter().filter(|s| s.playing).collect();

    json!({
        "groups": data.groups.iter().map(|g| json!({
            "slot": g.slot, "name": g.name, "members": g.members,
        })).collect::<Vec<_>>(),

        // A checksum manifest, not the packages themselves — which is what the
        // reference emits here, and why this document is kilobytes rather than
        // megabytes. A tracker fetches the real data separately and caches it
        // by checksum.
        "datapackage": data.datapackage.iter().map(|(game, checksum)| {
            // `version` is 0 for every game that has a checksum, which is every
            // game since 0.3.9. Kept because the reference emits both keys.
            (game.clone(), json!({ "checksum": checksum, "version": 0 }))
        }).collect::<serde_json::Map<String, Value>>(),

        "player_locations_total": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot, "total_locations": s.total_locations,
        })).collect::<Vec<_>>(),

        "player_game": players.iter().map(|s| json!({
            "team": s.team, "player": s.slot, "game": s.game,
        })).collect::<Vec<_>>(),
    })
}

/// `[item, location, player, flags]`.
///
/// An array rather than an object, because the reference's `NetworkItem` is a
/// `NamedTuple` and Flask renders those as tuples. pahoa's own `NetworkItem`
/// serializes as a *map* — that is what the WebSocket protocol wants — so this
/// cannot reuse it.
fn item(i: &pahoa_proto::NetworkItem) -> Value {
    json!([i.item, i.location, i.player, i.flags])
}

/// `[receiving_player, finding_player, location, item, found, entrance,
/// item_flags, status]`, in `Hint`'s declaration order.
fn hint(h: &pahoa_multidata::Hint) -> Value {
    json!([
        h.receiving_player,
        h.finding_player,
        h.location,
        h.item,
        h.found,
        h.entrance,
        h.item_flags,
        h.status as i64,
    ])
}

/// RFC 1123, as the reference emits — `Mon, 17 Aug 2026 18:22:09 GMT` — and
/// `null` for a slot that has not acted.
///
/// Not RFC 3339, which is what `/admin/v1/status` uses. The two surfaces answer
/// to different contracts and this one's is the reference's.
fn rfc1123(at: Option<f64>) -> Value {
    let Some(secs) = at else {
        return Value::Null;
    };
    let secs = secs as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // 1970-01-01 was a Thursday.
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekday = DAYS[days.rem_euclid(7) as usize];

    // Civil-from-days, as in `http::rfc3339`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    Value::String(format!(
        "{weekday}, {day:02} {} {year:04} {hh:02}:{mm:02}:{ss:02} GMT",
        MONTHS[(month - 1) as usize]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: f64) -> Value {
        rfc1123(Some(secs))
    }

    #[test]
    fn timestamps_match_the_references_rfc_1123() {
        // The exact string from a live archipelago.gg tracker document, at the
        // instant it names.
        assert_eq!(at(1_786_990_929.0), json!("Mon, 17 Aug 2026 18:22:09 GMT"));
        assert_eq!(at(0.0), json!("Thu, 01 Jan 1970 00:00:00 GMT"));
        // A leap day, and every weekday around it.
        assert_eq!(at(1_709_164_800.0), json!("Thu, 29 Feb 2024 00:00:00 GMT"));
        assert_eq!(at(1_709_251_200.0), json!("Fri, 01 Mar 2024 00:00:00 GMT"));
    }

    /// Null, never a zero timestamp: "has not connected" is not "connected at
    /// the epoch", and a tracker renders the two very differently.
    #[test]
    fn a_slot_that_has_not_acted_reports_null() {
        assert_eq!(rfc1123(None), Value::Null);
    }

    #[test]
    fn items_and_hints_are_arrays_in_declaration_order() {
        let i = pahoa_proto::NetworkItem {
            item: 80001,
            location: 16_871_244_510,
            player: 71,
            flags: 1,
        };
        assert_eq!(item(&i), json!([80001, 16_871_244_510i64, 71, 1]));

        let h = pahoa_multidata::Hint {
            receiving_player: 1,
            finding_player: 85,
            location: 641,
            item: 80000,
            found: true,
            entrance: String::new(),
            item_flags: 1,
            status: pahoa_multidata::HintStatus::Found,
        };
        // Byte for byte what a live tracker document contains.
        assert_eq!(hint(&h), json!([1, 85, 641, 80000, true, "", 1, 40]));
    }
}
