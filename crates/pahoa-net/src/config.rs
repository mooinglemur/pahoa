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
    pub shards: Option<usize>,

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
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: 38281,
            worker_threads: None,
            shards: None,
            outbound_budget_bytes: 512 * 1024 * 1024,
            per_connection_budget_bytes: 256 * 1024,
            max_frame_bytes: 1024 * 1024,
            handshake_timeout: Duration::from_secs(30),
            deflate: crate::ws::handshake::DeflateConfig::default(),
            compression_level: 6,
            max_message_bytes: 4 * 1024 * 1024,
            max_header_bytes: 16 * 1024,
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

    pub fn accept_config(&self) -> crate::ws::accept::AcceptConfig {
        crate::ws::accept::AcceptConfig {
            deflate: self.deflate,
            max_headers: self.max_header_bytes,
            timeout: self.handshake_timeout,
            max_message: self.max_message_bytes,
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
    const CONNECTIONS_PER_SLOT: usize = 3;
    const FLOOR: usize = 64 * 1024 * 1024;

    slots
        .saturating_mul(CONNECTIONS_PER_SLOT)
        .saturating_mul(PER_CONNECTION)
        .max(FLOOR)
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

/// Whole CPUs available to this cgroup, rounded up.
fn cgroup_cpu_quota() -> Option<usize> {
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
