//! What `/admin/v1/status` and `/admin/v1/metrics` report.
//!
//! Two renderings of one snapshot. The JSON is for a person or an orchestrator
//! reading a single room; the Prometheus text is for a scraper that wants the
//! same numbers over time. Neither computes anything the other does not — they
//! differ only in shape.

use super::rfc3339;
use pahoa_multidata::MultiData;
use std::time::{Duration, SystemTime};

/// The half of a status document only the actor can see.
#[derive(Debug, Clone)]
pub struct Status {
    pub clients_connected: usize,
    /// State has changed since the last save was *started*.
    pub save_dirty: bool,
    pub save_interval: Duration,
    /// Whether this room persists at all. A room with no `--save-dir` reports
    /// its save block as absent rather than as a save that never happens.
    pub saving: bool,
    pub options: Options,
    pub slots: Vec<SlotStatus>,
    /// Unix seconds when any slot last registered a new location check, or
    /// `None` if none ever has. See [`pahoa_room::Room::last_check_at`].
    pub last_check_at: Option<f64>,
}

/// The room's rules, as they are *now*.
///
/// Worth reading from here rather than from whatever configured the room.
/// These are the fields the save is authoritative for: after the first save the
/// room's own copy wins over any flag it was started with, and `!admin /option`
/// can move them mid-game. A room that has been up for a week may legitimately
/// disagree with its own manifest.
///
/// The three passwords are deliberately absent. They are the mirror image —
/// never saved, always from configuration — and there is nothing to learn here
/// that `/api/v1/room`'s `password_required` does not already say without
/// disclosing a secret to anything that reads a status document.
#[derive(Debug, Clone)]
pub struct Options {
    pub hint_cost: u32,
    pub location_check_points: u32,
    pub release_mode: &'static str,
    pub collect_mode: &'static str,
    pub remaining_mode: &'static str,
    pub countdown_mode: &'static str,
    pub item_cheat: bool,
    pub compatibility: u8,
}

#[derive(Debug, Clone)]
pub struct SlotStatus {
    pub slot: u32,
    pub name: String,
    pub game: String,
    /// Open connections for this slot, which is commonly more than one.
    pub connections: usize,
    pub checks: usize,
    pub total_checks: usize,
    pub status: &'static str,
    /// Barred from connecting by an administrator. Persisted, and independent
    /// of every password mode.
    pub locked: bool,
    /// Whether anything is filtering this slot's traffic, from its own rules or
    /// the room's.
    ///
    /// A boolean rather than the rules themselves: a 2000-slot room would
    /// otherwise repeat the room-wide filter two thousand times in a document
    /// that is already 2.7 MB. `/admin/v1/slots/<n>/filter` has the detail.
    pub filtered: bool,
}

/// The JSON document.
pub fn document(
    seed: &MultiData,
    live: &Status,
    started_at: SystemTime,
    outbound_budget_bytes: usize,
) -> serde_json::Value {
    let (save_micros, save_bytes) = crate::metrics::last_save();

    serde_json::json!({
        "seed_name": seed.seed_name,
        "pahoa_version": env!("CARGO_PKG_VERSION"),
        "api_version": 1,
        "started_at": rfc3339(started_at),

        "save": if live.saving {
            serde_json::json!({
                "last_save_at": crate::metrics::last_save_at().map(rfc3339),
                "last_save_bytes": save_bytes,
                "last_save_micros": save_micros.as_micros() as u64,
                "save_interval_seconds": live.save_interval.as_secs(),
                "dirty": live.save_dirty,
            })
        } else {
            // Explicitly null rather than absent: "this room keeps nothing" is
            // a state worth reporting, and a missing key reads as a bug.
            serde_json::Value::Null
        },

        "net": {
            "clients_connected": live.clients_connected,
            "mailbox_depth": crate::metrics::mailbox_depth(),
            "mailbox_peak": crate::metrics::mailbox_peak(),
            "lag_disconnects": crate::metrics::lag_disconnects(),
            "outbound_queued_bytes": crate::budget::queued_bytes(),
            "outbound_peak_bytes": crate::budget::peak_bytes(),
            "outbound_budget_bytes": outbound_budget_bytes,
            "resident_bytes": crate::metrics::resident_bytes(),
            "compressions": crate::ws::deflate::compressions(),
        },

        // Two different questions, and an orchestrator needs both. The first
        // pair answers "is this socket set alive"; the second answers "is
        // anyone still *playing*", which is what an idle reaper is really
        // asking and what the reference shuts rooms down on.
        "activity": {
            "last_client_message_at": crate::metrics::last_client_message_at().map(rfc3339),
            "idle_seconds": idle_seconds(),
            "last_check_at": live.last_check_at.map(room_time).map(rfc3339),
            "check_idle_seconds": check_idle_seconds(live.last_check_at),
        },

        "filters": {
            "slots_filtered": live.slots.iter().filter(|s| s.filtered).count(),
            "dropped_from_slots": pahoa_room::filter::dropped_from_slot(),
            "dropped_to_slots": pahoa_room::filter::dropped_to_slot(),
        },

        "options": {
            "hint_cost": live.options.hint_cost,
            "location_check_points": live.options.location_check_points,
            "release_mode": live.options.release_mode,
            "collect_mode": live.options.collect_mode,
            "remaining_mode": live.options.remaining_mode,
            "countdown_mode": live.options.countdown_mode,
            "item_cheat": live.options.item_cheat,
            "compatibility": live.options.compatibility,
        },

        "slots": live.slots.iter().map(|s| serde_json::json!({
            "slot": s.slot,
            "name": s.name,
            "game": s.game,
            "connected": s.connections > 0,
            "connections": s.connections,
            "checks": s.checks,
            "total_checks": s.total_checks,
            "status": s.status,
            "locked": s.locked,
            "filtered": s.filtered,
        })).collect::<Vec<_>>(),
    })
}

/// Prometheus text exposition, hand-rendered.
///
/// A fixed set of numbers into a fixed format is not worth a client library,
/// and the format is stable enough that writing it out is the whole job.
pub fn prometheus(live: &Status, outbound_budget_bytes: usize) -> String {
    let (save_micros, save_bytes) = crate::metrics::last_save();
    let mut out = String::with_capacity(2048);

    let mut metric = |name: &str, help: &str, kind: &str, value: u64| {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
        ));
    };

    metric(
        "pahoa_clients_connected",
        "Open client connections, including those that have not authenticated.",
        "gauge",
        live.clients_connected as u64,
    );
    metric(
        "pahoa_mailbox_depth",
        "Messages queued for the room actor. The bottleneck canary.",
        "gauge",
        crate::metrics::mailbox_depth() as u64,
    );
    metric(
        "pahoa_mailbox_peak",
        "Deepest the actor mailbox has been since startup.",
        "gauge",
        crate::metrics::mailbox_peak() as u64,
    );
    metric(
        "pahoa_lag_disconnects_total",
        "Connections the room decided to drop for falling behind. Counts the decision; \
         the close itself is forced out of band. Should be zero in a healthy room.",
        "counter",
        crate::metrics::lag_disconnects(),
    );
    metric(
        "pahoa_outbound_queued_bytes",
        "Bytes queued for clients across all connections.",
        "gauge",
        crate::budget::queued_bytes() as u64,
    );
    metric(
        "pahoa_outbound_peak_bytes",
        "Most that has ever been queued at once.",
        "gauge",
        crate::budget::peak_bytes() as u64,
    );
    metric(
        "pahoa_outbound_budget_bytes",
        "Ceiling on queued outbound bytes.",
        "gauge",
        outbound_budget_bytes as u64,
    );
    metric(
        "pahoa_compressions_total",
        "Messages compressed. Should track broadcasts, not broadcasts times connections.",
        "counter",
        crate::ws::deflate::compressions(),
    );
    if let Some(rss) = crate::metrics::resident_bytes() {
        metric(
            "pahoa_resident_bytes",
            "Resident set size of the whole process, allocator included.",
            "gauge",
            rss,
        );
    }
    metric(
        "pahoa_idle_seconds",
        "Seconds since any client last sent a message.",
        "gauge",
        idle_seconds().unwrap_or(0),
    );
    // The JSON reports `null` here for a room nobody has played yet; the text
    // exposition has no null, so a scraper sees 0 and must read it with
    // `pahoa_checks_total` to tell "just checked" from "never checked".
    metric(
        "pahoa_check_idle_seconds",
        "Seconds since any slot last registered a new location check. 0 when \
         none ever has, which `pahoa_checks_total` disambiguates.",
        "gauge",
        check_idle_seconds(live.last_check_at).unwrap_or(0),
    );

    if live.saving {
        metric(
            "pahoa_save_bytes",
            "Size of the most recent save.",
            "gauge",
            save_bytes,
        );
        metric(
            "pahoa_save_duration_microseconds",
            "Wall time of the most recent save.",
            "gauge",
            save_micros.as_micros() as u64,
        );
        metric(
            "pahoa_save_dirty",
            "1 when state has changed since the last save started.",
            "gauge",
            u64::from(live.save_dirty),
        );
        if let Some(at) = crate::metrics::last_save_at() {
            metric(
                "pahoa_last_save_timestamp_seconds",
                "Unix time of the last completed save.",
                "gauge",
                at.duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            );
        }
    }

    metric(
        "pahoa_slots",
        "Player slots in this seed.",
        "gauge",
        live.slots.len() as u64,
    );
    metric(
        "pahoa_slots_connected",
        "Player slots with at least one open connection.",
        "gauge",
        live.slots.iter().filter(|s| s.connections > 0).count() as u64,
    );
    // Worth a gauge rather than only a status field: a lock is meant to be
    // temporary, and the failure mode is nobody remembering to lift it.
    metric(
        "pahoa_slots_locked",
        "Slots an administrator has barred from connecting.",
        "gauge",
        live.slots.iter().filter(|s| s.locked).count() as u64,
    );
    metric(
        "pahoa_slots_filtered",
        "Slots with something filtering their traffic, their own rules or the room's.",
        "gauge",
        live.slots.iter().filter(|s| s.filtered).count() as u64,
    );
    metric(
        "pahoa_filtered_from_slots_total",
        "Messages dropped because a slot's filter matched what it sent.",
        "counter",
        pahoa_room::filter::dropped_from_slot(),
    );
    metric(
        "pahoa_filtered_to_slots_total",
        "Messages dropped because a filter matched what a slot would receive. \
         Counted per recipient, so one broadcast filtered for forty slots is forty.",
        "counter",
        pahoa_room::filter::dropped_to_slot(),
    );
    metric(
        "pahoa_checks_total",
        "Locations checked across every slot.",
        "gauge",
        live.slots.iter().map(|s| s.checks as u64).sum(),
    );
    metric(
        "pahoa_checks_possible",
        "Locations that exist across every slot.",
        "gauge",
        live.slots.iter().map(|s| s.total_checks as u64).sum(),
    );

    out
}

/// Seconds since any client last said anything, or `None` if none has.
fn idle_seconds() -> Option<u64> {
    let at = crate::metrics::last_client_message_at()?;
    Some(
        SystemTime::now()
            .duration_since(at)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

/// The room's clock is unix seconds as an `f64`; this surface speaks RFC 3339.
///
/// Clamped at the epoch because a negative or non-finite instant is not a time
/// anyone can act on, and `as u64` on either is a silently absurd number.
fn room_time(at: f64) -> SystemTime {
    let secs = if at.is_finite() { at.max(0.0) } else { 0.0 };
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
}

/// Seconds since any slot last checked a location, or `None` if none ever has.
///
/// `None` is load-bearing and must not become a zero: a room that nobody has
/// played yet and a room somebody checked a location in this second are
/// opposite states, and an orchestrator reaping on this reads them differently.
fn check_idle_seconds(last_check_at: Option<f64>) -> Option<u64> {
    let at = room_time(last_check_at?);
    Some(
        SystemTime::now()
            .duration_since(at)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The room's clock is unix seconds; puna parses RFC 3339.
    #[test]
    fn a_room_timestamp_renders_as_rfc3339() {
        assert_eq!(rfc3339(room_time(1_700_000_500.0)), "2023-11-14T22:21:40Z");
    }

    /// Fractional seconds truncate rather than rounding up, so a rendered time
    /// never claims a check happened later than it did.
    #[test]
    fn fractional_seconds_truncate() {
        assert_eq!(rfc3339(room_time(1_700_000_500.9)), "2023-11-14T22:21:40Z");
    }

    /// A clock that has gone strange must not render as a time far in the
    /// future: `as u64` on a negative or non-finite `f64` is a garbage number,
    /// and this surface is read by a reaper that would act on it.
    #[test]
    fn an_impossible_clock_clamps_to_the_epoch() {
        for bad in [-1.0, f64::NAN, f64::NEG_INFINITY, f64::INFINITY] {
            assert_eq!(
                rfc3339(room_time(bad)),
                "1970-01-01T00:00:00Z",
                "{bad} should not render as a usable time"
            );
        }
    }

    /// **`None` must stay `None` all the way to the wire.**
    ///
    /// Puna distinguishes "nobody has checked anything yet" from "somebody
    /// checked at the epoch" and reaps on them differently: the first means a
    /// room still filling up, and collapsing it into a zero would have puna
    /// stopping rooms whose organizer is mid-setup.
    #[test]
    fn a_room_with_no_checks_reports_null_not_zero() {
        assert_eq!(check_idle_seconds(None), None);
    }

    #[test]
    fn check_idle_counts_from_the_last_check() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs() as f64;
        let idle = check_idle_seconds(Some(now - 90.0)).expect("a check happened");
        assert!((88..=92).contains(&idle), "idle was {idle}");
    }

    /// A clock slightly ahead of the wall reads as "just now", not as a huge
    /// number wrapped from a negative duration.
    #[test]
    fn a_check_in_the_future_reads_as_zero() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs() as f64;
        assert_eq!(check_idle_seconds(Some(now + 3600.0)), Some(0));
    }
}
