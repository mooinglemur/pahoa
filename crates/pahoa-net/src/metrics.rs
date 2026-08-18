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

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
