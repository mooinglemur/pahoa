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

/// Wire bytes of the protocol messages a slot **sent**.
///
/// Frame bytes off the socket — header, mask and compressed payload — so this
/// is comparable with [`DELIVERED`]'s outbound bytes rather than with the
/// inflated text the room parses. A `Set` from a tracker that arrives
/// compressed counts what it cost to carry, not what it expanded to.
///
/// **Only messages carrying client packets.** Pings, pongs, binary frames and
/// anything that fails to decode are excluded, because the reader task that
/// sees those bytes does not know which slot to charge them to and the actor
/// never learns they happened. That makes this exactly the byte counterpart of
/// [`PACKETS`], with the same attribution and the same pre-auth split, and the
/// help text says so rather than implying it covers the socket.
static BYTES_IN: LazyLock<RwLock<HashMap<Option<pahoa_room::SlotKey>, AtomicU64>>> =
    LazyLock::new(RwLock::default);

/// Count one message read from a client.
///
/// `slot` is resolved when the message arrives, before any of its packets are
/// handled — so a frame carrying `Connect` is pre-auth even though handling it
/// is what creates the slot, matching [`record_packet`].
pub fn record_bytes_in(slot: Option<pahoa_room::SlotKey>, bytes: usize) {
    if let Some(count) = BYTES_IN.read().expect("not poisoned").get(&slot) {
        count.fetch_add(bytes as u64, Ordering::Relaxed);
        return;
    }
    BYTES_IN
        .write()
        .expect("not poisoned")
        .entry(slot)
        .or_default()
        .fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Every slot that has sent anything, with its byte total.
pub fn bytes_in() -> Vec<(Option<pahoa_room::SlotKey>, u64)> {
    BYTES_IN
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(slot, count)| (*slot, count.load(Ordering::Relaxed)))
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

/// Connections that authenticated, by whether they negotiated
/// permessage-deflate.
///
/// **The question is which games' clients support compression**, and answering
/// it needs two facts that are settled in different places: the extension is
/// negotiated during the WebSocket handshake, before `Connect`, so no game is
/// known yet; and the game arrives with `Connect`, known only to the room. They
/// meet on the shard's `Member`, which is where this is counted.
///
/// **Per connection, not per slot.** A slot's clients can differ — a game
/// client may compress while a tracker on the same slot does not — so the
/// connection is the honest unit, and `sum by (game, deflate)` is the panel.
///
/// A counter rather than a gauge: cumulative survives churn and answers the
/// question over a room's life, where a gauge of currently-connected would
/// need decrementing on every disconnect to say anything at all.
static DEFLATE: LazyLock<RwLock<HashMap<(pahoa_room::SlotKey, bool), AtomicU64>>> =
    LazyLock::new(RwLock::default);

/// Count one connection reaching a slot, and whether it compresses.
pub fn record_client_deflate(key: pahoa_room::SlotKey, deflate: bool) {
    if let Some(count) = DEFLATE.read().expect("not poisoned").get(&(key, deflate)) {
        count.fetch_add(1, Ordering::Relaxed);
        return;
    }
    DEFLATE
        .write()
        .expect("not poisoned")
        .entry((key, deflate))
        .or_default()
        .fetch_add(1, Ordering::Relaxed);
}

/// Every observed (slot, deflate) pair with its count.
pub fn client_deflate() -> Vec<((pahoa_room::SlotKey, bool), u64)> {
    DEFLATE
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(key, count)| (*key, count.load(Ordering::Relaxed)))
        .collect()
}

/// The HTTP surface, counted apart from the game.
///
/// **Deliberately separate from the WebSocket traffic above**, even though both
/// arrive on the same port and through the same accept path. They are different
/// workloads with different operators: one is players, the other is an
/// orchestrator polling on a reconcile loop, plus whatever the internet points
/// at a public port. Summing them would hide a scraper behind a busy room, and
/// hide a busy room behind a scraper.
///
/// A WebSocket upgrade is *not* counted here. It is an HTTP request in form
/// only, and everything it goes on to carry is already the game's.
static HTTP: LazyLock<RwLock<HashMap<HttpKey, Exchange>>> = LazyLock::new(RwLock::default);

/// One row of the HTTP table.
///
/// `route` is a **template**, not the path as sent: `/admin/v1/slots/7/filter`
/// counts under `/admin/v1/slots/{slot}/filter`, and anything unrecognized
/// under `other`. A public port gets scanned, and a label taken from the
/// request line would let a scanner mint series until the scrape fell over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HttpKey {
    pub route: &'static str,
    pub method: &'static str,
    pub status: u16,
}

#[derive(Debug, Default)]
struct Exchange {
    count: AtomicU64,
    request_bytes: AtomicU64,
    response_bytes: AtomicU64,
}

/// The methods worth a label, mapped to `'static` so the table cannot be grown
/// by inventing verbs.
fn known_method(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "other",
    }
}

/// Count one HTTP request and its answer.
pub fn record_http(
    route: &'static str,
    method: &str,
    status: u16,
    request_bytes: usize,
    response_bytes: usize,
) {
    let key = HttpKey {
        route,
        method: known_method(method),
        status,
    };
    let bump = |e: &Exchange| {
        e.count.fetch_add(1, Ordering::Relaxed);
        e.request_bytes
            .fetch_add(request_bytes as u64, Ordering::Relaxed);
        e.response_bytes
            .fetch_add(response_bytes as u64, Ordering::Relaxed);
    };
    if let Some(e) = HTTP.read().expect("not poisoned").get(&key) {
        bump(e);
        return;
    }
    bump(HTTP.write().expect("not poisoned").entry(key).or_default());
}

/// Every observed (route, method, status) with its count and byte totals.
pub fn http() -> Vec<(HttpKey, u64, u64, u64)> {
    HTTP.read()
        .expect("not poisoned")
        .iter()
        .map(|(key, e)| {
            (
                *key,
                e.count.load(Ordering::Relaxed),
                e.request_bytes.load(Ordering::Relaxed),
                e.response_bytes.load(Ordering::Relaxed),
            )
        })
        .collect()
}

/// Requests that never parsed into anything, so they have no route to file
/// under. A port scan looks like this.
static HTTP_MALFORMED: AtomicU64 = AtomicU64::new(0);
/// Admin credentials that were wrong or missing.
static AUTH_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Requests refused because the source had already failed too often.
static AUTH_RATE_LIMITED: AtomicU64 = AtomicU64::new(0);

pub fn record_http_malformed() {
    HTTP_MALFORMED.fetch_add(1, Ordering::Relaxed);
}

/// One wrong or missing admin token.
///
/// Worth its own counter rather than reading it off
/// `pahoa_http_requests_total{status="401"}`: that number also carries the
/// tracker's gate, and this is the one an operator alerts on.
pub fn record_auth_failure() {
    AUTH_FAILURES.fetch_add(1, Ordering::Relaxed);
}

pub fn record_auth_rate_limited() {
    AUTH_RATE_LIMITED.fetch_add(1, Ordering::Relaxed);
}

pub fn http_malformed() -> u64 {
    HTTP_MALFORMED.load(Ordering::Relaxed)
}

pub fn auth_failures() -> u64 {
    AUTH_FAILURES.load(Ordering::Relaxed)
}

pub fn auth_rate_limited() -> u64 {
    AUTH_RATE_LIMITED.load(Ordering::Relaxed)
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
