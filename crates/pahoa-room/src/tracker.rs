//! What the tracker API reads, taken off the room in one cheap pass.
//!
//! The document a tracker renders is large — measured at 2.7 MB for a 185-slot
//! room — and rendering it is pure CPU. So this is deliberately *not* the JSON:
//! it is a snapshot of `Arc`s, taken while the actor holds `&mut Room` and
//! costing a refcount bump per slot, which the HTTP task then serializes on its
//! own thread. The same division `Room::snapshot` makes for saving, for the same
//! reason.
//!
//! See `docs/tracker.md` for the shape this feeds and why it mirrors the
//! reference exactly.

use pahoa_multidata::{ClientStatus, Hint};
use pahoa_proto::NetworkItem;
use std::collections::HashSet;
use std::sync::Arc;

/// One slot's tracked state.
#[derive(Debug, Clone)]
pub struct TrackerSlot {
    pub team: u32,
    pub slot: u32,
    pub game: String,
    /// `None` when the player has not set one, which the API reports as null.
    pub alias: Option<String>,
    pub status: ClientStatus,
    pub total_locations: usize,
    /// Shared with the room; never copied unless the room writes to it next.
    pub checks: Arc<HashSet<i64>>,
    /// The *remote* queue, which is the one the reference's tracker reads.
    pub items_received: Arc<Vec<NetworkItem>>,
    pub hints: Arc<Vec<Hint>>,
    /// Unix seconds, or `None` if it has never happened. Both survive a
    /// restart, because an async outlives the process serving it.
    pub last_activity: Option<f64>,
    pub last_connection: Option<f64>,
}

/// An item-link or co-op group, which the static document lists separately.
#[derive(Debug, Clone)]
pub struct TrackerGroup {
    pub slot: u32,
    pub name: String,
    pub members: Vec<u32>,
}

/// Everything both tracker endpoints need, snapshotted together.
#[derive(Debug, Clone)]
pub struct TrackerData {
    pub slots: Vec<TrackerSlot>,
    pub groups: Vec<TrackerGroup>,
    /// `{game: (checksum, version)}` — the manifest the reference emits, not
    /// the packages themselves.
    pub datapackage: Vec<(String, Option<String>)>,
    /// Locations checked across every slot, per team.
    pub total_checks: Vec<(u32, usize)>,
}
