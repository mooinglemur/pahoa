//! Runtime and transport sizing.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub bind: String,
    pub port: u16,

    /// Tokio worker threads. `None` derives from the cgroup CPU quota.
    pub worker_threads: Option<usize>,

    /// Number of fan-out shards. `None` matches the worker thread count.
    ///
    /// Broadcasts cost the room actor one message per shard rather than one per
    /// connection, which is the whole point of having them.
    ///
    /// A room that knows its seed should call [`shards_for`] instead. The
    /// worker-thread fallback here is a last resort for a caller that has no
    /// slot count — it sizes a *topology* parameter from a *compute* one, and
    /// the two do not track each other. See [`shards_for`].
    pub shards: Option<usize>,

    /// How many messages one shard's frame inbox holds. `None` matches
    /// [`DEFAULT_SHARD_QUEUE_DEPTH`].
    ///
    /// A room that knows its seed should call [`shard_queue_depth_for`]: the
    /// cost of draining one broadcast scales with the shard's membership, so a
    /// depth that absorbs a burst in a four-slot room is not the same number
    /// that absorbs one in a two-thousand-slot room.
    ///
    /// **This memory is not inside [`NetConfig::outbound_budget_bytes`].** The
    /// budget is charged where a frame is queued *for a connection*, which is
    /// downstream of here; a message waiting in this inbox has not been
    /// expanded to an audience yet and nothing has reserved for it. See
    /// [`shard_queue_bytes`] for what to size a container against.
    pub shard_queue_depth: Option<usize>,

    /// Total bytes that may sit in outbound queues across all connections.
    ///
    /// A **global** cap with a small per-connection share, not a large
    /// per-connection budget: at 6000 connections an 8 MiB each would be 48 GB.
    ///
    /// The default here is the 2000-slot figure. A room that knows its seed
    /// should call [`outbound_budget_for`] instead, so the cap means something
    /// for a small room rather than sitting far above anything reachable.
    pub outbound_budget_bytes: usize,
    pub per_connection_budget_bytes: usize,

    /// Largest inbound frame accepted, matching the reference server's
    /// `websockets` default (`max_size=2**20`).
    pub max_frame_bytes: usize,

    /// How long a connection may take to send its first frame.
    pub handshake_timeout: Duration,

    /// How often to send a WebSocket Ping on each connection. Zero disables.
    ///
    /// **The server is the only side that pings.** Archipelago's own clients
    /// connect with `ping_interval=None` (`CommonClient.py:872`), explicitly
    /// turning theirs off, so a room that does not ping leaves an idle
    /// connection completely silent in both directions. Middleboxes reap silent
    /// flows — commonly at 60s — and neither end is told, which produces a
    /// connection both sides still believe in. Observed in the wild: a browser
    /// client that pings survived where a custom client that did not was
    /// dropped, from the same machine over the same path.
    ///
    /// 20 seconds matches the reference, which inherits it from `websockets`
    /// (`ping_interval=20`). It sits far enough under a 60s idle timeout to
    /// survive one comfortably.
    pub ping_interval: Duration,

    /// How long to wait for the matching Pong before dropping the connection.
    /// Zero keeps pinging but never judges the answer.
    ///
    /// **Not an allowance for a lost ping.** TCP retransmits, so a ping cannot
    /// vanish the way a datagram heartbeat can; one outstanding probe is a
    /// sufficient test and "three strikes" would only add latency. This is
    /// headroom for the peer's *application* to turn the frame around — a
    /// single-threaded client inside a long frame, a congested path, a client
    /// whose own receive queue is behind.
    ///
    /// It is also the only signal available. Writing a ping to a dead peer
    /// *succeeds*: the bytes land in the local send buffer and TCP retries for
    /// minutes, so the write never reports the failure. The absent pong is the
    /// whole of the evidence.
    ///
    /// Worst-case detection is `ping_interval + ping_timeout`, since a peer that
    /// dies just after answering is not probed again for a full interval.
    pub ping_timeout: Duration,

    /// permessage-deflate negotiation.
    pub deflate: crate::ws::handshake::DeflateConfig,

    /// Deflate level for outbound frames, 0-9.
    ///
    /// 6 rather than 1 because the trade is lopsided: a broadcast is compressed
    /// once and its bytes go to every connection, so ratio multiplies by the
    /// connection count while CPU does not. Measured on a full 140-packet
    /// chunk, level 6 costs 175µs against level 1's 87µs and produces 3.6 KiB
    /// against 8.7 KiB — across a mass release at 6000 connections that is
    /// 63 GB versus 149 GB. Level 9 buys a further 0.9% for 73% more time.
    pub compression_level: u32,

    /// Largest inbound message after decompression.
    ///
    /// Separate from `max_frame_bytes` because a 2 KiB deflate window still
    /// expands far enough to matter: the frame cap bounds what arrives, this
    /// bounds what it turns into.
    pub max_message_bytes: usize,

    /// Header bytes accepted before the request is refused.
    pub max_header_bytes: usize,

    /// Body bytes accepted on an HTTP request.
    ///
    /// Generous for the largest admin command — a `say` with a long message is
    /// still under a kilobyte — and small enough that a public endpoint cannot
    /// be made to buffer on anyone's say-so.
    pub max_body_bytes: usize,

    /// Certificate and key to terminate TLS with. `None` serves plaintext only,
    /// answering a ClientHello with a `handshake_failure` alert so a client that
    /// probes `wss://` first falls back at once.
    pub tls: Option<crate::tls::TlsPaths>,

    /// Keep serving `ws://` on the same port after a certificate is configured.
    ///
    /// Off by default and deliberately opt-in. The admin API is mutating,
    /// internet-reachable, and guarded by nothing but a bearer token; serving
    /// that token's traffic in cleartext alongside the TLS it was meant to
    /// travel under would undo the point of having it. The byte sniff still
    /// runs either way, so a plaintext client gets an immediate `426` rather
    /// than a hang.
    pub allow_plaintext: bool,

    /// A second listener serving the scoped feed.
    ///
    /// `None` runs one port. Both ports terminate the same TLS and serve the
    /// same HTTP surface; only the WebSocket feed differs, and the port is what
    /// decides which a client gets — because the clients that need the quiet
    /// feed cannot select a tag or a path. See `docs/scoped-feed.md`.
    pub filtered_port: Option<u16>,

    /// Serve the tracker without authentication even when an admin token is
    /// configured.
    ///
    /// Off by default. With a token set, the tracker is gated behind it — an
    /// open tracker on a public port lets an anonymous port scan read the
    /// participant list out of every room, which is what a room without a
    /// password relies on staying hidden. A standalone pahoa with no token
    /// serves it openly regardless, which is the case the CORS headers exist
    /// for. See `docs/tracker.md`.
    pub open_tracker: bool,

    /// Bearer token for `/admin/v1/**`.
    ///
    /// `None` makes the admin surface answer `404` — absent rather than merely
    /// locked, so a misconfiguration fails closed and is indistinguishable from
    /// a build that never had one.
    pub admin_token: Option<String>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: 38281,
            worker_threads: None,
            shards: None,
            shard_queue_depth: None,
            outbound_budget_bytes: 512 * 1024 * 1024,
            per_connection_budget_bytes: 256 * 1024,
            max_frame_bytes: 1024 * 1024,
            handshake_timeout: Duration::from_secs(30),
            // The reference's values, by way of `websockets`' defaults.
            ping_interval: Duration::from_secs(20),
            ping_timeout: Duration::from_secs(20),
            deflate: crate::ws::handshake::DeflateConfig::default(),
            compression_level: 6,
            max_message_bytes: 4 * 1024 * 1024,
            max_header_bytes: 16 * 1024,
            max_body_bytes: 64 * 1024,
            tls: None,
            allow_plaintext: false,
            filtered_port: None,
            open_tracker: false,
            admin_token: None,
        }
    }
}

impl NetConfig {
    pub fn worker_threads_resolved(&self) -> usize {
        self.worker_threads.unwrap_or_else(detect_worker_threads)
    }

    pub fn shards_resolved(&self) -> usize {
        self.shards
            .unwrap_or_else(|| self.worker_threads_resolved())
            .max(1)
    }

    pub fn shard_queue_depth_resolved(&self) -> usize {
        self.shard_queue_depth
            .unwrap_or(DEFAULT_SHARD_QUEUE_DEPTH)
            .max(1)
    }

    pub fn accept_config(&self) -> crate::ws::accept::AcceptConfig {
        crate::ws::accept::AcceptConfig {
            deflate: self.deflate,
            max_headers: self.max_header_bytes,
            timeout: self.handshake_timeout,
            max_message: self.max_message_bytes,
            max_body: self.max_body_bytes,
        }
    }
}

/// The outbound budget for a room of this many slots.
///
/// The cap exists so a room survives clients that stop reading, so the size
/// that makes sense follows the connection count — which follows the seed, not
/// a constant. Players commonly run a game client plus a text client plus a
/// tracker, so this sizes for **three connections per slot**, the same rule the
/// rest of the design uses.
///
/// The per-connection allowance below is deliberately *under*
/// [`NetConfig::per_connection_budget_bytes`]: if the global cap were simply
/// `connections × 256 KiB` it could never bind before every individual cap did,
/// and it would stop being a backstop at all. M9 measured a **333 MiB** peak
/// across 6000 connections through a mass release — about 58 KiB each — so
/// 96 KiB leaves real headroom above the worst case actually observed while
/// still catching a runaway.
///
/// The floor keeps a small room's cap from producing false lag disconnects: a
/// 4-slot room can only ever queue 12 × 256 KiB = 3 MiB, so 64 MiB is twenty
/// times its true worst case. It is a limit, not an allocation — nothing is
/// reserved — so a generous floor costs nothing and a too-tight one costs
/// disconnects that the client did not deserve.
pub fn outbound_budget_for(slots: usize) -> usize {
    /// Headroom per expected connection, under the per-connection cap so the
    /// global limit still binds first when many clients stall at once.
    const PER_CONNECTION: usize = 96 * 1024;
    const FLOOR: usize = 64 * 1024 * 1024;

    slots
        .saturating_mul(CONNECTIONS_PER_SLOT)
        .saturating_mul(PER_CONNECTION)
        .max(FLOOR)
}

/// Connections one slot is expected to bring: a game client, a text client and
/// a tracker. The same rule [`outbound_budget_for`] sizes against.
const CONNECTIONS_PER_SLOT: usize = 3;

/// What a shard's frame inbox holds when nothing sizes it from a seed, and the
/// floor [`shard_queue_depth_for`] will not go below. It is the constant every
/// room ran at before either knob existed, and it has never been the thing that
/// failed on its own.
pub const DEFAULT_SHARD_QUEUE_DEPTH: usize = 4096;

/// Widest fan-out worth asking for. See [`shards_for`] for why there is a
/// ceiling at all.
pub const MAX_SHARDS: usize = 32;

/// Deepest inbox worth asking for. Past this the envelopes alone cost megabytes
/// per shard, and a room this far behind will not catch up by queuing more.
pub const MAX_SHARD_QUEUE_DEPTH: usize = 65536;

/// Fan-out width for a room of this many slots.
///
/// # Why this does not follow the CPU quota
///
/// It used to, and that was wrong twice over. Shard count is a **topology**
/// decision — it follows how many connections there are to fan out to — while
/// the worker count is a **compute** one, and an orchestrator that sets
/// `limits.cpu: 2` for a 2000-slot room means exactly that. Deriving one from
/// the other left the only way to widen the fan-out being to buy a CPU ceiling
/// nothing was going to use.
///
/// # It is a reliability parameter before it is a throughput one
///
/// `Shards::broadcast` answers a full inbox by closing **every connection the
/// shard owns**, because the audience is expanded inside the shard and the
/// actor that dropped the message does not know who it was for. Blast radius is
/// therefore `connections / shards`, and at the old default a 2000-slot room on
/// two workers put half the room behind one queue. A dev-cluster run at ~5000
/// connections shed ~2,500 of them on the first overflow and never recovered:
/// every one came back at once, each buying a full item-history replay, which
/// costs the room far more than shedding them saved.
///
/// # The ceiling
///
/// Each shard compresses each broadcast at most once for its own deflate
/// connections, so compression work is `O(shards)` per broadcast, not
/// `O(connections)` — that is what makes fan-out cheap. But it does mean shards
/// are not free: past some width the redundant compressions cost more than the
/// narrower blast radius buys. 32 is the same ceiling
/// [`detect_worker_threads`] clamps to, and at that width a 6000-slot room
/// still keeps its blast radius near the target below.
pub fn shards_for(slots: usize) -> usize {
    /// Connections one shard may own before it is worth splitting. This is the
    /// blast radius a broadcast overflow costs, so it is chosen as "how many
    /// players may a dropped frame disconnect", not as a throughput figure.
    const CONNECTIONS_PER_SHARD: usize = 512;
    const FLOOR: usize = 2;

    slots
        .saturating_mul(CONNECTIONS_PER_SLOT)
        .div_ceil(CONNECTIONS_PER_SHARD)
        .clamp(FLOOR, MAX_SHARDS)
}

/// Frame-inbox depth for a room of this many slots at this fan-out width.
///
/// # Two burst shapes, and only one of them divides by the width
///
/// This took the width as its only input once, and that was wrong in a way that
/// made the whole derivation inert. The two bursts a shard has to absorb scale
/// in opposite directions:
///
/// - **A reconnect storm is per-connection.** Every connection comes back at
///   once, each buying its own replay, so what one shard sees is a burst from
///   the connections *it* owns — and widening the fan-out really does lower
///   what each shard needs, by the same factor.
/// - **A release tail is per-broadcast, and does not divide by anything.**
///   [`crate::shard::Shards::broadcast`] puts one copy of the message into
///   *every* shard's inbox, so the broadcasts a room may have outstanding is
///   exactly the depth, however many shards there are. Widening the fan-out
///   buys no broadcast headroom at all — it only multiplies what the same
///   headroom costs in memory.
///
/// Sizing for the first alone made the two knobs **anti-correlated** for the
/// burst that was actually binding: dividing the depth by a width that buys
/// broadcasts nothing moved the scarce number *down* as the fan-out widened.
///
/// It was also inert on the default path, which is the part that hid it.
/// [`shards_for`] divides by 512 and this multiplied by 8, and those cancel
/// exactly: every room up to ~5,461 slots computed at or below
/// [`DEFAULT_SHARD_QUEUE_DEPTH`], the floor won, and the answer was always
/// 4,096. The derivation only ever contributed once [`MAX_SHARDS`] clamped the
/// divisor. The test that was supposed to cover this pinned the width by hand,
/// which is the one case that does not collapse.
///
/// So the depth is the **larger** of the two shapes.
///
/// # Why the release burst is one broadcast per receiver slot
///
/// A mass release amortizes on the full feed — 140 items to one broadcast — but
/// the scoped feed cannot: it emits one broadcast per distinct receiver slot,
/// because each one carries only what concerns that slot. A release therefore
/// costs about `min(locations per slot, slots)` broadcasts, which is why
/// `effect.rs` puts a 2000-slot release at ~2,860 broadcast frames. Bounding it
/// by the slot count is an upper bound available before the room starts.
///
/// Sixteen concurrent releases is puna's figure, arrived at independently on
/// the cluster this failed on and adopted here unchanged so that a room and its
/// orchestrator cannot disagree about what it was sized for.
///
/// # The ceiling binds above ~4,096 slots
///
/// [`MAX_SHARD_QUEUE_DEPTH`] caps this, so past ~4,096 slots the number of
/// concurrent releases covered falls below sixteen — ten at 6,000 slots. That
/// is deliberate: the envelopes cost `shards × depth`, so the ceiling is what
/// keeps a wide fan-out from reserving hundreds of megabytes for headroom that
/// only ever needed to be one deep queue's worth. See [`shard_queue_bytes`].
pub fn shard_queue_depth_for(slots: usize, shards: usize) -> usize {
    /// Headroom per connection one shard owns, for the reconnect storm.
    const MESSAGES_PER_CONNECTION: usize = 8;
    /// Simultaneous releases to keep room for, for the release tail. Each costs
    /// up to one broadcast per receiver slot, and a broadcast occupies a slot
    /// in every shard's inbox.
    const CONCURRENT_RELEASES: usize = 16;

    let reconnect_storm = slots
        .saturating_mul(CONNECTIONS_PER_SLOT)
        .div_ceil(shards.max(1))
        .saturating_mul(MESSAGES_PER_CONNECTION);
    let release_tail = slots.saturating_mul(CONCURRENT_RELEASES);

    reconnect_storm
        .max(release_tail)
        .clamp(DEFAULT_SHARD_QUEUE_DEPTH, MAX_SHARD_QUEUE_DEPTH)
}

/// Memory the shard inboxes can hold, which **nothing else accounts for**.
///
/// [`NetConfig::outbound_budget_bytes`] is charged when a frame is queued for a
/// *connection*, which happens after a shard has expanded the audience — so a
/// message still sitting in a shard's inbox is outside the budget entirely. An
/// orchestrator sizing a container against the budget has to add this.
///
/// This is the **envelope** cost, and it is the part that is bounded: `depth ×
/// shards` messages of [`ShardMsg`], all of it reserved by `mpsc::channel` up
/// front. The payloads they point at are refcounted `Bytes` — one broadcast is
/// a single allocation no matter how many shards hold it — so the payload
/// footprint is bounded by what the room has in flight rather than by the
/// depth, and a queue this deep is only reached when the room is producing far
/// more than it drains.
///
/// [`ShardMsg`]: crate::shard::ShardMsg
pub fn shard_queue_bytes(shards: usize, depth: usize) -> usize {
    shards
        .saturating_mul(depth)
        .saturating_mul(size_of::<crate::shard::ShardMsg>())
}

/// Worker-thread count derived from the cgroup CPU quota, not the host.
///
/// `available_parallelism()` reports the *machine's* cores, which in Kubernetes
/// is routinely the node rather than the container. On a 64-core node with
/// `limits.cpu: 2` that would spawn 64 workers fighting over two CPUs' worth of
/// quota, plus ~128 MiB of thread stacks. Clamped to `[2, 32]`: the ceiling is
/// deliberately higher than a small room needs, because 6000 connections' worth
/// of I/O and fan-out shards have to fit under it.
pub fn detect_worker_threads() -> usize {
    let quota = cgroup_cpu_quota();
    let host = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    quota.unwrap_or(host).clamp(2, 32)
}

/// Bytes this cgroup may use, if it is capped.
///
/// Reported in the startup banner rather than used for anything. It is the
/// number `outbound_budget_for` is implicitly betting against, so having both
/// on one line is what makes an OOM kill diagnosable after the fact.
pub fn cgroup_memory_limit() -> Option<u64> {
    // cgroup v2 spells "no limit" as the literal `max`.
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        return s.trim().parse().ok();
    }
    // cgroup v1 spells it as a number near u64::MAX, which is not a limit
    // anybody set. Anything past a petabyte is that sentinel, not a cap.
    let v: u64 = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (v < (1 << 50)).then_some(v)
}

/// Whole CPUs available to this cgroup, rounded up.
pub fn cgroup_cpu_quota() -> Option<usize> {
    // cgroup v2: "<quota> <period>", or "max <period>" when unlimited.
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut parts = s.split_whitespace();
        let quota = parts.next()?;
        let period: f64 = parts.next()?.parse().ok()?;
        if quota != "max"
            && let Ok(q) = quota.parse::<f64>()
            && period > 0.0
        {
            return Some((q / period).ceil().max(1.0) as usize);
        }
        return None;
    }

    // cgroup v1.
    let quota: i64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let period: i64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if quota <= 0 || period <= 0 {
        return None;
    }
    Some(((quota as f64 / period as f64).ceil()).max(1.0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_threads_stay_within_the_clamp() {
        let n = detect_worker_threads();
        assert!((2..=32).contains(&n), "got {n}");
    }

    #[test]
    fn shards_default_to_the_worker_count_and_are_never_zero() {
        let cfg = NetConfig::default();
        assert_eq!(cfg.shards_resolved(), cfg.worker_threads_resolved());

        let explicit = NetConfig {
            shards: Some(0),
            ..Default::default()
        };
        assert_eq!(
            explicit.shards_resolved(),
            1,
            "zero shards would drop every broadcast"
        );
    }

    #[test]
    fn the_queue_depth_falls_back_to_the_constant_and_is_never_zero() {
        let cfg = NetConfig::default();
        assert_eq!(cfg.shard_queue_depth_resolved(), DEFAULT_SHARD_QUEUE_DEPTH);

        let explicit = NetConfig {
            shard_queue_depth: Some(0),
            ..Default::default()
        };
        assert_eq!(
            explicit.shard_queue_depth_resolved(),
            1,
            "a zero-capacity channel refuses every send, so every broadcast \
             would overflow and close the room"
        );
    }

    /// **The blast radius is what this is sizing**, so that is what to assert
    /// on: a broadcast that overflows closes every connection its shard owns.
    #[test]
    fn the_fan_out_bounds_what_one_dropped_broadcast_can_close() {
        // The width the old default produced for the room that fell over: two
        // CPUs meant two shards, so half of a 2000-slot room sat behind one
        // queue. Nothing about the seed changed; only what it is derived from.
        for slots in [500usize, 2000, 6000] {
            let shards = shards_for(slots);
            let blast = slots * 3 / shards;
            assert!(
                blast <= 600,
                "{slots} slots across {shards} shards puts {blast} connections \
                 behind one queue"
            );
        }

        // Small rooms keep some parallelism without paying for shards they
        // cannot fill, and nothing exceeds the documented ceiling.
        assert_eq!(shards_for(0), 2);
        assert_eq!(shards_for(4), 2);
        assert_eq!(shards_for(usize::MAX), MAX_SHARDS);
    }

    /// **The derivation has to contribute on the path rooms actually take.**
    ///
    /// It did not. `shards_for` divides by 512 and the depth multiplied the
    /// per-shard connection count by 8, and those cancel exactly — so every
    /// room up to ~5,461 slots computed at or below the floor, the floor won,
    /// and the answer was always 4,096 however many slots the seed had. The
    /// formula only ever contributed once `MAX_SHARDS` clamped the divisor.
    ///
    /// It survived review because the test that was meant to cover it pinned
    /// the width by hand, which is the one case that does not collapse. This
    /// one takes the width from `shards_for`, the way a room does.
    #[test]
    fn the_default_path_derives_a_depth_rather_than_landing_on_the_floor() {
        for slots in [500usize, 1000, 2000, 6000] {
            let depth = shard_queue_depth_for(slots, shards_for(slots));
            assert!(
                depth > DEFAULT_SHARD_QUEUE_DEPTH,
                "{slots} slots defaulted to the floor, so the derivation did \
                 nothing: {depth}"
            );
        }

        // A room small enough that the floor is genuinely the right answer
        // still gets it, and that is not the same failure.
        assert_eq!(
            shard_queue_depth_for(4, shards_for(4)),
            DEFAULT_SHARD_QUEUE_DEPTH
        );
    }

    /// **Widening the fan-out must never shrink the broadcast headroom.**
    ///
    /// `Shards::broadcast` puts one copy of the message into *every* shard's
    /// inbox, so the broadcasts a room may have outstanding is exactly the
    /// depth — the width buys none. Deriving the depth by dividing by the width
    /// therefore made the two knobs anti-correlated for the burst that was
    /// actually binding: going from 2 shards to 12 cut the scarce number by six
    /// while multiplying what it cost in memory by the same factor.
    ///
    /// A release is what produces that burst. The full feed amortizes 140 items
    /// into one broadcast; the scoped feed cannot, because each broadcast
    /// carries only what concerns one receiver slot.
    #[test]
    fn the_broadcast_headroom_does_not_fall_as_the_fan_out_widens() {
        const SLOTS: usize = 2000;
        // Sixteen concurrent releases at one broadcast per receiver slot.
        let needed = SLOTS * 16;
        for shards in 1..=MAX_SHARDS {
            let depth = shard_queue_depth_for(SLOTS, shards);
            assert!(
                depth >= needed,
                "at {shards} shards a {SLOTS}-slot room holds {depth} broadcasts, \
                 under the {needed} a release tail needs — and the width bought \
                 none of it back"
            );
        }
    }

    /// The other shape still wins where it should: a fan-out pinned narrow
    /// leaves each shard owning enough connections that a reconnect storm —
    /// every one of them returning at once, each buying a full replay — is the
    /// larger of the two bursts.
    #[test]
    fn a_fan_out_pinned_narrow_is_sized_for_the_reconnect_storm() {
        let narrow = shard_queue_depth_for(2000, 1);
        assert!(
            narrow > 2000 * 16,
            "one shard owning 6000 connections needs more than the release \
             tail alone: {narrow}"
        );

        // The failure from the dev cluster, in the arithmetic. Two shards owned
        // ~3000 connections each against the flat 4096, which is 1.4 messages
        // per connection — less than one broadcast apiece.
        assert!(
            shard_queue_depth_for(2000, 2) >= 2000 * 3 / 2,
            "a shard must be able to hold at least one message per connection \
             it owns"
        );

        // A small room stays exactly where it has always been.
        assert_eq!(shard_queue_depth_for(4, 2), DEFAULT_SHARD_QUEUE_DEPTH);
        assert_eq!(
            shard_queue_depth_for(usize::MAX, 1),
            MAX_SHARD_QUEUE_DEPTH,
            "the ceiling binds rather than overflowing"
        );
    }

    /// What an orchestrator sizing a container has to add to the outbound
    /// budget, since nothing else accounts for it.
    ///
    /// **This is not free and the numbers here are the price of the fix.**
    /// Sizing the depth for the release tail rather than letting it collapse to
    /// the floor took a 2000-slot room from 3.4 MiB of envelopes to 26 MiB, and
    /// a 6000-slot one to the 144 MiB ceiling. That is the trade: broadcast
    /// headroom costs `shards × depth`, because a broadcast occupies a slot in
    /// every shard's inbox, while buying headroom of only `depth`.
    ///
    /// The bound asserted is the one at the documented flag limits, so it holds
    /// for anything an operator can ask for and not merely for what the
    /// derivation picks.
    #[test]
    fn the_shard_inboxes_cost_a_bounded_amount_at_every_reachable_sizing() {
        // Nothing reachable through the flags exceeds the corner.
        let ceiling = shard_queue_bytes(MAX_SHARDS, MAX_SHARD_QUEUE_DEPTH);
        assert!(
            ceiling <= 160 * 1024 * 1024,
            "the worst case an operator can ask for is {ceiling} bytes"
        );

        for slots in [4usize, 500, 2000, 6000] {
            let shards = shards_for(slots);
            let bytes = shard_queue_bytes(shards, shard_queue_depth_for(slots, shards));
            assert!(
                bytes <= ceiling,
                "{slots} slots reserve {bytes} bytes, past the documented corner"
            );
        }

        // A small room is still nearly free, which is what keeps a hand-run
        // pahoa from reserving tens of megabytes it will never touch.
        let small = shard_queue_bytes(shards_for(4), shard_queue_depth_for(4, shards_for(4)));
        assert!(
            small < 1024 * 1024,
            "a four-slot room reserves {small} bytes"
        );

        assert_eq!(shard_queue_bytes(usize::MAX, usize::MAX), usize::MAX);
    }

    #[test]
    fn the_budget_follows_the_seed_and_never_drops_below_the_floor() {
        let floor = 64 * 1024 * 1024;
        // A small room cannot reach even its floor: 12 connections at the
        // 256 KiB per-connection cap is 3 MiB, so this can never false-positive.
        assert_eq!(outbound_budget_for(4), floor);
        assert_eq!(outbound_budget_for(0), floor);

        // The design target. Landing near the hand-picked 512 MiB it replaces
        // is the point: that number was about right for 2000 slots and wrong
        // for everything else.
        let big = outbound_budget_for(2000);
        assert!(
            (512 * 1024 * 1024..=1024 * 1024 * 1024).contains(&big),
            "2000 slots gave {big} bytes"
        );

        // And it must stay under what the per-connection caps alone would
        // allow, or the global cap stops being a backstop.
        let cfg = NetConfig::default();
        let individually = 2000 * 3 * cfg.per_connection_budget_bytes;
        assert!(big < individually, "{big} vs {individually}");
    }

    #[test]
    fn a_huge_slot_count_does_not_overflow_the_budget() {
        // Slot counts come from a file on disk. Saturating rather than wrapping
        // matters: a wrapped budget would be a tiny one, and every client would
        // be dropped as too slow.
        assert!(outbound_budget_for(usize::MAX) > 0);
    }

    #[test]
    fn per_connection_budget_is_small_enough_to_scale() {
        // The check that stops someone "helpfully" raising this to 8 MiB: at
        // 6000 connections that would be 48 GB of headroom.
        let cfg = NetConfig::default();
        let worst_case = cfg.per_connection_budget_bytes * 6000;
        assert!(
            worst_case <= 4 * 1024 * 1024 * 1024,
            "per-connection budget implies {worst_case} bytes at 6000 connections"
        );
    }
}
