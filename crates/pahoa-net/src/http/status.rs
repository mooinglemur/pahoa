//! What `/admin/v1/status` and `/admin/v1/metrics` report.
//!
//! Two renderings of one snapshot. The JSON is for a person or an orchestrator
//! reading a single room; the Prometheus text is for a scraper that wants the
//! same numbers over time. Neither computes anything the other does not — they
//! differ only in shape.

use super::rfc3339;
use pahoa_multidata::MultiData;
use pahoa_room::SlotKey;
use std::collections::HashMap;
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
    /// Which team this row is. One team exists, so it is always the same value
    /// — reported anyway, because a caller that reads it will not need changing
    /// on the day there is more than one, and one that infers it will.
    pub team: u32,
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
            "team": s.team,
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
         Counted per recipient connection, so one broadcast filtered for forty slots is forty, \
         and eighty if each also has a tracker attached. pahoa_filtered_from_slots_total is \
         counted once per message instead, because that is where its decision is.",
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

    metric(
        "pahoa_admin_auth_failures_total",
        "Admin requests with a wrong or missing bearer token. Its own counter rather than a \
         status filter, because pahoa_http_requests_total{status=\"401\"} also carries the \
         tracker's gate.",
        "counter",
        crate::metrics::auth_failures(),
    );
    metric(
        "pahoa_admin_auth_rate_limited_total",
        "Admin requests answered 429 because that source had already failed too often. A \
         correct token is never refused, so this counts only sources that were guessing.",
        "counter",
        crate::metrics::auth_rate_limited(),
    );
    metric(
        "pahoa_http_malformed_total",
        "Requests that never parsed into a route, so they are counted nowhere else. A port \
         scan looks like this.",
        "counter",
        crate::metrics::http_malformed(),
    );

    // The closure holds `out` mutably; nothing above needs it again.
    let _ = metric;
    process(&mut out);
    by_slot(&mut out, live);
    http_surface(&mut out);
    out
}

/// What the process costs the node it runs on.
///
/// **These two keep Prometheus's conventional names rather than the `pahoa_`
/// prefix**, because they are the two every client library exports and every
/// off-the-shelf dashboard already plots. `rate(process_cpu_seconds_total[5m])`
/// is CPU cores used, and it works on a panel nobody had to write.
///
/// The older `pahoa_resident_bytes` is the same idea under a house name and is
/// left alone: renaming it would break a scrape that already exists for no gain
/// beyond tidiness. New process-level metrics take the convention; that one
/// stays where puna can still find it.
///
/// Both are absent rather than zero when `/proc` cannot be read, which is the
/// honest answer on a platform that does not have it — a zero here would read
/// as an idle room.
fn process(out: &mut String) {
    if let Some(seconds) = crate::metrics::cpu_seconds() {
        out.push_str(
            "# HELP process_cpu_seconds_total Total user and system CPU time spent in seconds. \
             Process-wide: it says what this room costs a node, not which task is busy — \
             pahoa_mailbox_depth is what says whether the actor is the bottleneck.\n\
             # TYPE process_cpu_seconds_total counter\n",
        );
        out.push_str(&format!("process_cpu_seconds_total {seconds:.2}\n"));
    }
    if let Some(at) = crate::metrics::start_time_seconds() {
        out.push_str(
            "# HELP process_start_time_seconds Start time of the process since unix epoch in \
             seconds.\n\
             # TYPE process_start_time_seconds gauge\n",
        );
        out.push_str(&format!("process_start_time_seconds {at:.3}\n"));
    }
    if let Some(rss) = crate::metrics::resident_bytes() {
        out.push_str(
            "# HELP process_resident_memory_bytes Resident memory size in bytes. The canonical \
             spelling of pahoa_resident_bytes, which is the same number and is kept for now.\n\
             # TYPE process_resident_memory_bytes gauge\n",
        );
        out.push_str(&format!("process_resident_memory_bytes {rss}\n"));
    }
}

/// The HTTP surface, kept apart from the game's traffic.
///
/// Same port, different workload: an orchestrator on a reconcile loop and
/// whatever the internet points at a public listener, against players. Summed
/// together, each would hide the other.
fn http_surface(out: &mut String) {
    let mut rows = crate::metrics::http();
    if rows.is_empty() {
        return;
    }
    rows.sort_unstable_by_key(|(key, _, _, _)| (key.route, key.method, key.status));

    out.push_str(
        "# HELP pahoa_http_requests_total Requests answered on the HTTP surface, by route, \
         method and status. WebSocket upgrades are not counted here. The route is a template, \
         so a slot's filter counts under /admin/v1/slots/{slot}/filter and anything \
         unrecognized under \"other\" — a public port gets scanned, and a label taken from the \
         request line would let a scanner mint series.\n\
         # TYPE pahoa_http_requests_total counter\n",
    );
    for (key, count, _, _) in &rows {
        out.push_str(&format!(
            "pahoa_http_requests_total{{route=\"{}\",method=\"{}\",status=\"{}\"}} {count}\n",
            label(key.route),
            key.method,
            key.status
        ));
    }

    // Summed by route: the method and status of a request say little about what
    // it weighed, and a tracker document is megabytes where a health check is
    // bytes.
    for (name, help, pick) in [
        (
            "pahoa_http_request_bytes_total",
            "Bytes received on the HTTP surface, head and body, by route.",
            0,
        ),
        (
            "pahoa_http_response_bytes_total",
            "Bytes sent on the HTTP surface, head and body, by route. The tracker documents \
             dominate this on a large room.",
            1,
        ),
    ] {
        let mut by_route: Vec<(&'static str, u64)> = Vec::new();
        for (key, _, request_bytes, response_bytes) in &rows {
            let value = if pick == 0 {
                request_bytes
            } else {
                response_bytes
            };
            match by_route.iter_mut().find(|(route, _)| *route == key.route) {
                Some((_, total)) => *total += value,
                None => by_route.push((key.route, *value)),
            }
        }
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
        for (route, total) in by_route {
            out.push_str(&format!("{name}{{route=\"{}\"}} {total}\n", label(route)));
        }
    }
}

/// The labeled series: traffic and drops broken out per slot.
///
/// Kept apart from the fixed metrics above because these are the only ones
/// whose *number of series* depends on the room. Sorted before rendering — the
/// tables behind them are hash maps, and a scrape whose line order changed
/// every tick would be unreadable in a diff and gratuitously hard to test.
fn by_slot(out: &mut String, live: &Status) {
    let named: HashMap<SlotKey, (&str, &str)> = live
        .slots
        .iter()
        .map(|s| ((s.team, s.slot), (s.name.as_str(), s.game.as_str())))
        .collect();

    // `player` and `game` are functions of the key — one each — so all four
    // together are one dimension of size "slots in this room" rather than the
    // product four labels look like. They travel with the slot so a dashboard
    // can group by game without joining against a roster.
    //
    // **`team` is here even though it is always `0`.** A room has one team and
    // is refused if its seed says otherwise, so this label carries no
    // information today — but a scraper that already groups by it needs nothing
    // rewritten if that ever changes, and one that assumed slot numbers were
    // unique would silently add two teams together. Cardinality is unaffected:
    // it is a function of the key like the other two.
    let identify = |key: SlotKey| {
        let (name, game) = named.get(&key).copied().unwrap_or(("", ""));
        // A spectator plays nothing, and `Archipelago` is what the datapackage
        // already calls that — a value rather than a hole, so nothing has to
        // special-case an empty label.
        let game = if game.is_empty() { "Archipelago" } else { game };
        format!(
            "team=\"{}\",slot=\"{}\",player=\"{}\",game=\"{}\"",
            key.0,
            key.1,
            label(name),
            label(game)
        )
    };

    let mut packets = crate::metrics::packets();
    packets.sort_unstable_by_key(|(row, _)| (row.key, row.cmd));

    if packets.iter().any(|(row, _)| row.key.is_some()) {
        out.push_str(
            "# HELP pahoa_packets_in_total Packets received from a slot, by command. \
             Only pairs actually observed appear.\n\
             # TYPE pahoa_packets_in_total counter\n",
        );
        for (row, count) in &packets {
            if let Some(key) = row.key {
                out.push_str(&format!(
                    "pahoa_packets_in_total{{{},cmd=\"{}\"}} {count}\n",
                    identify(key),
                    label(row.cmd)
                ));
            }
        }
    }

    // Separate rather than the same counter with the labels left empty: every
    // per-slot aggregation would otherwise have to remember to exclude it.
    if packets.iter().any(|(row, _)| row.key.is_none()) {
        out.push_str(
            "# HELP pahoa_packets_preauth_total Packets received before the connection held a \
             slot, by command. Connect and GetDataPackage are the only two the room answers \
             unauthenticated, so a climbing count here is failed logins.\n\
             # TYPE pahoa_packets_preauth_total counter\n",
        );
        for (row, count) in &packets {
            if row.key.is_none() {
                out.push_str(&format!(
                    "pahoa_packets_preauth_total{{cmd=\"{}\"}} {count}\n",
                    label(row.cmd)
                ));
            }
        }
    }

    let mut deflate = crate::metrics::client_deflate();
    if !deflate.is_empty() {
        deflate.sort_unstable_by_key(|((key, on), _)| (*key, *on));
        out.push_str(
            "# HELP pahoa_client_connections_total Connections that reached a slot, by whether \
             they negotiated permessage-deflate. Per connection, not per slot: a game client \
             may compress where a tracker on the same slot does not. sum by (game, deflate) is \
             which games' clients support it.\n\
             # TYPE pahoa_client_connections_total counter\n",
        );
        for ((key, on), count) in &deflate {
            out.push_str(&format!(
                "pahoa_client_connections_total{{{},deflate=\"{on}\"}} {count}\n",
                identify(*key)
            ));
        }
    }

    let mut bytes_in = crate::metrics::bytes_in();
    bytes_in.sort_unstable_by_key(|(slot, _)| *slot);

    if bytes_in.iter().any(|(slot, _)| slot.is_some()) {
        out.push_str(
            "# HELP pahoa_bytes_in_total Wire bytes of the protocol messages a slot sent, as \
             framed and compressed on the socket. Pings, pongs and undecodable frames are not \
             included: the reader that sees those bytes does not know whose they are.\n\
             # TYPE pahoa_bytes_in_total counter\n",
        );
        for (slot, count) in &bytes_in {
            if let Some(key) = slot {
                out.push_str(&format!(
                    "pahoa_bytes_in_total{{{}}} {count}\n",
                    identify(*key)
                ));
            }
        }
    }

    if let Some((_, count)) = bytes_in.iter().find(|(slot, _)| slot.is_none()) {
        out.push_str(
            "# HELP pahoa_bytes_in_preauth_total Wire bytes of messages read before the \
             connection held a slot. A Connect arrives here, so this climbing on its own is \
             login attempts.\n\
             # TYPE pahoa_bytes_in_preauth_total counter\n",
        );
        out.push_str(&format!("pahoa_bytes_in_preauth_total {count}\n"));
    }

    // What the room produced, once per message whatever its audience. No slot
    // label: a slot's connections are not sent the same stream, so there is no
    // honest one — see `crate::metrics::PACKETS_OUT`.
    let mut produced = crate::metrics::packets_out();
    if !produced.is_empty() {
        produced.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out.push_str(
            "# HELP pahoa_packets_out_total Packets the room emitted, by command. Counted once \
             per message whatever its audience, so one broadcast to two thousand slots is one; \
             pahoa_frames_out_total is what fan-out made of it.\n\
             # TYPE pahoa_packets_out_total counter\n",
        );
        for (cmd, count) in &produced {
            out.push_str(&format!(
                "pahoa_packets_out_total{{cmd=\"{}\"}} {count}\n",
                label(cmd)
            ));
        }
    }

    let mut delivered = crate::metrics::deliveries();
    delivered.sort_unstable_by_key(|(slot, _, _)| *slot);

    if delivered.iter().any(|(slot, _, _)| slot.is_some()) {
        for (name, help, pick) in [
            (
                "pahoa_frames_out_total",
                "WebSocket frames handed to a slot's writers. Per recipient connection, so a \
                 slot with a game and two trackers counts three for a broadcast all three \
                 receive.",
                0,
            ),
            (
                "pahoa_bytes_out_total",
                "Bytes handed to a slot's writers, after compression. Per recipient connection, \
                 like pahoa_frames_out_total. This is what fills the outbound budget.",
                1,
            ),
        ] {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
            for (slot, frames, bytes) in &delivered {
                if let Some(key) = slot {
                    let value = if pick == 0 { frames } else { bytes };
                    out.push_str(&format!("{name}{{{}}} {value}\n", identify(*key)));
                }
            }
        }
    }

    // Pre-auth is not nothing: every connection that opens is sent `RoomInfo`,
    // and a `DataPackage` answered before a slot is known runs to megabytes.
    if let Some((_, frames, bytes)) = delivered.iter().find(|(slot, _, _)| slot.is_none()) {
        out.push_str(
            "# HELP pahoa_frames_out_preauth_total Frames sent to connections that held no slot \
             yet.\n# TYPE pahoa_frames_out_preauth_total counter\n",
        );
        out.push_str(&format!("pahoa_frames_out_preauth_total {frames}\n"));
        out.push_str(
            "# HELP pahoa_bytes_out_preauth_total Bytes sent to connections that held no slot \
             yet. RoomInfo goes to every connection that opens, and a DataPackage answered here \
             can be megabytes.\n# TYPE pahoa_bytes_out_preauth_total counter\n",
        );
        out.push_str(&format!("pahoa_bytes_out_preauth_total {bytes}\n"));
    }

    let mut drops = pahoa_room::filter::drops_by_slot();
    if !drops.is_empty() {
        drops.sort_unstable_by_key(|(row, _)| {
            (row.key, row.direction.as_text(), row.kind.as_text())
        });
        out.push_str(
            "# HELP pahoa_filtered_total Messages a filter dropped, by slot, direction and kind. \
             Sums to pahoa_filtered_from_slots_total and pahoa_filtered_to_slots_total, which are \
             this same table added up. The two directions have different denominators: from_slot \
             is once per message, where the room makes the decision, and to_slot is once per \
             recipient connection, matching pahoa_frames_out_total, so one chat line filtered for \
             forty slots is forty and eighty if each also has a tracker.\n\
             # TYPE pahoa_filtered_total counter\n",
        );
        for (row, count) in &drops {
            out.push_str(&format!(
                "pahoa_filtered_total{{{},direction=\"{}\",kind=\"{}\"}} {count}\n",
                identify(row.key),
                row.direction.as_text(),
                row.kind.as_text()
            ));
        }
    }
}

/// How long a label value may be before it is cut.
///
/// Player names and games come out of an uploaded seed, so they are untrusted
/// text of arbitrary length — a 4 KB name is expressible — and a label value
/// that size is a problem for whoever stores the scrape rather than for the
/// room. Generous enough that no real name reaches it.
const MAX_LABEL: usize = 128;

/// Escape a label value for the text exposition, and bound its length.
///
/// Backslash, double quote and newline are the three the format cannot carry
/// raw. Everything here is attacker-supplied in the sense that matters: a seed
/// is an uploaded zip, so a name containing a quote would otherwise end the
/// label early and put arbitrary text where a metric name goes.
fn label(value: &str) -> String {
    let mut end = value.len().min(MAX_LABEL);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end);
    for c in value[..end].chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
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
