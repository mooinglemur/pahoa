//! The handful of numbers that say whether the concurrency model is holding.
//!
//! Deliberately a fixed set of process-wide counters rather than a metrics
//! framework: these exist to answer specific questions the plan poses about
//! behavior at 6000 connections, and each one has a failure it is meant to
//! catch.
//!
//! - **actor mailbox depth** — the bottleneck canary. The single task owning
//!   room state is fine as long as its queue stays near empty; a depth that
//!   climbs and does not drain means work is arriving faster than the room can
//!   apply it, and everything else is downstream of that.
//! - **outbound bytes queued, and the peak** — against the global budget. Python
//!   buffers without limit here, which is unbounded memory growth; the whole
//!   point of the budget is that this number has a ceiling.
//! - **lag disconnects** — how often a client was dropped for falling behind.
//!   Should be zero in a healthy room, and is a deliberate divergence from the
//!   reference, so it needs to be visible rather than inferred.
//! - **compressions** — should track *broadcasts*, not broadcasts times
//!   connections. Lives in [`crate::ws::deflate`] next to the compressor.
//! - **save duration** — the last save's wall time, to confirm persistence stays
//!   off the critical path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, RwLock};

static MAILBOX_DEPTH: AtomicUsize = AtomicUsize::new(0);
static MAILBOX_PEAK: AtomicUsize = AtomicUsize::new(0);
static LAG_DISCONNECTS: AtomicU64 = AtomicU64::new(0);
static SAVE_MICROS: AtomicU64 = AtomicU64::new(0);
static SAVE_BYTES: AtomicU64 = AtomicU64::new(0);
/// Unix seconds of the last completed save. Zero means "none yet", which is
/// distinguishable from a real timestamp for any room started after 1970.
static SAVE_AT: AtomicU64 = AtomicU64::new(0);
/// Unix seconds of the last message from any client.
static LAST_CLIENT_MESSAGE_AT: AtomicU64 = AtomicU64::new(0);

pub fn record_mailbox_depth(depth: usize) {
    MAILBOX_DEPTH.store(depth, Ordering::Relaxed);
    MAILBOX_PEAK.fetch_max(depth, Ordering::Relaxed);
}

pub fn mailbox_depth() -> usize {
    MAILBOX_DEPTH.load(Ordering::Relaxed)
}

/// Deepest the actor's mailbox has ever been. The number that matters: a
/// transient spike is fine, a rising floor is not.
pub fn mailbox_peak() -> usize {
    MAILBOX_PEAK.load(Ordering::Relaxed)
}

pub fn record_lag_disconnect() {
    LAG_DISCONNECTS.fetch_add(1, Ordering::Relaxed);
}

pub fn lag_disconnects() -> u64 {
    LAG_DISCONNECTS.load(Ordering::Relaxed)
}

pub fn record_save(duration: std::time::Duration, bytes: usize) {
    SAVE_MICROS.store(duration.as_micros() as u64, Ordering::Relaxed);
    SAVE_BYTES.store(bytes as u64, Ordering::Relaxed);
    SAVE_AT.store(unix_now(), Ordering::Relaxed);
}

/// Wall time and size of the most recent save.
pub fn last_save() -> (std::time::Duration, u64) {
    (
        std::time::Duration::from_micros(SAVE_MICROS.load(Ordering::Relaxed)),
        SAVE_BYTES.load(Ordering::Relaxed),
    )
}

/// When the last save completed, or `None` if none has.
///
/// A wall clock rather than a duration because it is reported to an operator,
/// who wants "at 12:04" and not "1841 seconds ago" — and because a room that
/// has never saved has to be distinguishable from one that saved at startup.
pub fn last_save_at() -> Option<std::time::SystemTime> {
    match SAVE_AT.load(Ordering::Relaxed) {
        0 => None,
        secs => Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
    }
}

/// Note that a client said something. Called on the actor, once per batch.
///
/// A coarse timestamp on purpose: this exists to answer "is anyone still
/// playing", which an idle reaper acts on in minutes. A second's resolution and
/// a relaxed store cost nothing on the actor's hot path.
pub fn record_client_message() {
    LAST_CLIENT_MESSAGE_AT.store(unix_now(), Ordering::Relaxed);
}

/// Packets in, by the slot that sent them and what they were.
///
/// **The finest honest granularity, because it aggregates upward for free and
/// nothing recovers detail that was summed away.** `sum by (cmd)` is "incoming
/// packets by command"; joined to a slot's game it is "by game"; and neither of
/// those can answer the question a room actually gets asked, which is *which
/// slot* is producing the Bounce storm.
///
/// Sparse on purpose. A pair is created the first time it is observed, so a
/// slot that has never sent a `SetNotify` has no series rather than a zero —
/// on a 2000-slot room that is the difference between ~28,000 series and
/// something closer to a tenth of it, and a gap and a zero mean different
/// things on a dashboard anyway.
static PACKETS: LazyLock<RwLock<HashMap<PacketKey, AtomicU64>>> = LazyLock::new(RwLock::default);

/// One row of the inbound packet table.
///
/// `key` is `None` for a packet that arrived before the connection had a slot —
/// `Connect` and `GetDataPackage`, the only two the room answers unauthenticated
/// — which is reported separately rather than under an empty slot label. A room
/// being hammered by failed `Connect`s is a real thing to want to see, and the
/// alternative was a series every per-slot aggregation had to remember to
/// exclude.
///
/// A `(team, slot)` rather than a slot number, because that is what identifies
/// a participant; see `pahoa_multidata::MultiData::teams` for why there is only
/// ever one team behind it today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PacketKey {
    pub key: Option<pahoa_room::SlotKey>,
    pub cmd: &'static str,
}

/// Count one packet from a client.
///
/// Called on the actor, before the packet is handled — so a `Connect` is
/// attributed to nobody even though handling it is what gives the connection a
/// slot, which is the honest reading: it arrived before there was one.
pub fn record_packet(slot: Option<pahoa_room::SlotKey>, cmd: &'static str) {
    let key = PacketKey { key: slot, cmd };
    if let Some(count) = PACKETS.read().expect("not poisoned").get(&key) {
        count.fetch_add(1, Ordering::Relaxed);
        return;
    }
    PACKETS
        .write()
        .expect("not poisoned")
        .entry(key)
        .or_default()
        .fetch_add(1, Ordering::Relaxed);
}

/// Every observed (slot, command) pair with its count.
pub fn packets() -> Vec<(PacketKey, u64)> {
    PACKETS
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(key, count)| (*key, count.load(Ordering::Relaxed)))
        .collect()
}

/// Packets the room **produced**, by command.
///
/// Counted once per message when the room decides to emit it, whatever its
/// audience — so one chat line broadcast to two thousand slots is one. That is
/// the opposite convention to [`deliveries`] below, and deliberately: this
/// answers "what is the room generating", which is a property of the room, and
/// the two together say whether a load problem is production or fan-out.
///
/// **No slot label, because there is no honest one.** A slot's connections do
/// not receive the same stream — a `NoText` tracker is left out of chat, and a
/// scoped connection takes items through a different route than a full-feed one
/// — so "packets sent to slot 4" has no single value. Attributing per recipient
/// would also mean expanding every broadcast's audience on the actor, which is
/// the O(connections) walk the shards exist to avoid: a mass release is ~3,500
/// broadcasts.
///
/// Keyed by `String` so [`pahoa_proto::ServerPacket::cmd`] can borrow from an
/// `Echo`'s map. Only the first sighting of a command allocates.
static PACKETS_OUT: LazyLock<RwLock<HashMap<String, AtomicU64>>> = LazyLock::new(RwLock::default);

/// Count one packet the room is emitting.
pub fn record_packet_out(cmd: &str) {
    if let Some(count) = PACKETS_OUT.read().expect("not poisoned").get(cmd) {
        count.fetch_add(1, Ordering::Relaxed);
        return;
    }
    PACKETS_OUT
        .write()
        .expect("not poisoned")
        .entry(cmd.to_string())
        .or_default()
        .fetch_add(1, Ordering::Relaxed);
}

/// Every command the room has emitted, with its count.
pub fn packets_out() -> Vec<(String, u64)> {
    PACKETS_OUT
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(cmd, count)| (cmd.clone(), count.load(Ordering::Relaxed)))
        .collect()
}

/// Frames and bytes that actually reached a connection's writer, per slot.
///
/// **Per recipient connection**, so a slot with a game and two trackers counts
/// three times for a broadcast all three receive. That is the right convention
/// here even though it is the wrong one for [`PACKETS_OUT`]: these are bytes the
/// room really queued, they are what fills the outbound budget and what a lag
/// disconnect is downstream of, and dividing by connection count would not
/// recover a per-slot stream anyway — the connections of one slot are not sent
/// the same things.
///
/// Counted where the frame is handed over, so a delivery refused for lag or a
/// closed writer is not counted as sent.
static DELIVERED: LazyLock<RwLock<HashMap<Option<pahoa_room::SlotKey>, Delivered>>> =
    LazyLock::new(RwLock::default);

#[derive(Debug, Default)]
struct Delivered {
    frames: AtomicU64,
    bytes: AtomicU64,
}

/// Count one frame handed to a connection's writer.
///
/// `slot` is `None` before the connection authenticates, which is not nothing:
/// `RoomInfo` goes to every connection that opens, and a `DataPackage` answered
/// pre-auth can run to megabytes.
pub fn record_delivery(slot: Option<pahoa_room::SlotKey>, bytes: usize) {
    let bytes = bytes as u64;
    if let Some(d) = DELIVERED.read().expect("not poisoned").get(&slot) {
        d.frames.fetch_add(1, Ordering::Relaxed);
        d.bytes.fetch_add(bytes, Ordering::Relaxed);
        return;
    }
    let mut table = DELIVERED.write().expect("not poisoned");
    let d = table.entry(slot).or_default();
    d.frames.fetch_add(1, Ordering::Relaxed);
    d.bytes.fetch_add(bytes, Ordering::Relaxed);
}

/// Every slot that has been sent anything, with its frame and byte totals.
pub fn deliveries() -> Vec<(Option<pahoa_room::SlotKey>, u64, u64)> {
    DELIVERED
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(slot, d)| {
            (
                *slot,
                d.frames.load(Ordering::Relaxed),
                d.bytes.load(Ordering::Relaxed),
            )
        })
        .collect()
}

/// When a client last said anything, or `None` if none has since startup.
pub fn last_client_message_at() -> Option<std::time::SystemTime> {
    match LAST_CLIENT_MESSAGE_AT.load(Ordering::Relaxed) {
        0 => None,
        secs => Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resident set size, in bytes.
///
/// Read from `/proc` rather than tracked, because the question it answers —
/// "does memory scale with connection count the way the design says" — is about
/// the whole process, allocator included, not about what pahoa thinks it holds.
pub fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // `sysconf(_SC_PAGESIZE)` without libc: 4 KiB everywhere pahoa targets.
    Some(pages * 4096)
}

/// A one-line summary, for a load run or an operator poking at a live room.
pub fn summary() -> String {
    let (save_time, save_bytes) = last_save();
    let rss = resident_bytes().map_or("?".to_string(), |b| format!("{} MiB", b >> 20));
    format!(
        "mailbox {} (peak {}), outbound {} KiB (peak {} KiB), \
         lag disconnects {}, compressions {}, last save {:?}/{} KiB, rss {rss}",
        mailbox_depth(),
        mailbox_peak(),
        crate::budget::queued_bytes() >> 10,
        crate::budget::peak_bytes() >> 10,
        lag_disconnects(),
        crate::ws::deflate::compressions(),
        save_time,
        save_bytes >> 10,
    )
}
