//! WebSocket transport and the concurrency model.
//!
//! One task owns the room's mutable state; per-connection tasks handle parsing
//! and socket I/O; fan-out shards expand broadcasts so the actor never walks the
//! connection list. See [`actor`] for the invariant that keeps the single owner
//! from becoming a bottleneck, and [`shard`] for why broadcasts cost the actor a
//! handful of messages instead of thousands.

pub mod actor;
pub mod budget;
pub mod config;
pub mod http;
pub mod journal;
pub mod metrics;
pub mod save;
pub mod server;
pub mod shard;
pub mod tls;
pub mod ws;

pub use config::{
    DEFAULT_SHARD_QUEUE_DEPTH, MAX_SHARD_QUEUE_DEPTH, MAX_SHARDS, NetConfig, cgroup_cpu_quota,
    cgroup_memory_limit, detect_worker_threads, outbound_budget_for, shard_queue_bytes,
    shard_queue_depth_for, shards_for,
};
pub use save::{SaveSink, SaveStore};
pub use server::{Server, build_runtime, serve};
pub use tls::TlsPaths;
