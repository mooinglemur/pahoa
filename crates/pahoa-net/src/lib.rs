//! WebSocket transport and the concurrency model.
//!
//! One task owns the room's mutable state; per-connection tasks handle parsing
//! and socket I/O; fan-out shards expand broadcasts so the actor never walks the
//! connection list. See [`actor`] for the invariant that keeps the single owner
//! from becoming a bottleneck, and [`shard`] for why broadcasts cost the actor a
//! handful of messages instead of thousands.

pub mod actor;
pub mod config;
pub mod server;
pub mod shard;

pub use config::NetConfig;
pub use server::{Server, build_runtime, serve};
