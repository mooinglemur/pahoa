//! The Archipelago room state machine.
//!
//! Deliberately transport-agnostic: it consumes decoded [`pahoa_proto::ClientPacket`]s
//! and emits [`pahoa_proto::ServerPacket`]s through an [`EffectSink`], with no
//! sockets, no runtime and no clock of its own. That is what lets the whole of
//! the game logic — including a 400k-location release across 2000 slots — run in
//! a synchronous unit test, and lets the concurrency model change without
//! touching a single game rule.

mod conn;
mod datapackage;
mod effect;
pub mod fuzzy;
pub mod hints;
mod options;
mod room;
pub mod save;
pub mod secret;
mod slot_data;
pub mod tracker;

pub use conn::{Client, ConnId, FeedPolicy};
pub use effect::{
    CheckRecord, CloseReason, Counter, EffectSink, Event, JournalEvent, Recipients, Recorder,
};
pub use options::RoomOptions;
pub use room::{AdminCommand, AdminOutcome, Room, SERVER_VERSION, SlotKey};
pub use save::{SaveError, Snapshot};
