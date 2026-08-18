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
    pub slots: Vec<SlotStatus>,
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

        "activity": {
            "last_client_message_at": crate::metrics::last_client_message_at().map(rfc3339),
            "idle_seconds": idle_seconds(),
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
        "Connections dropped for falling behind. Should be zero in a healthy room.",
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
