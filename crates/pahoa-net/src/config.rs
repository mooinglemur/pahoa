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
    pub outbound_budget_bytes: usize,
    pub per_connection_budget_bytes: usize,

    /// Largest inbound frame accepted, matching the reference server's
    /// `websockets` default (`max_size=2**20`).
    pub max_frame_bytes: usize,

    /// How long a connection may take to send its first frame.
    pub handshake_timeout: Duration,
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
