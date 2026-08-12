//! Archipelago's network protocol: packet types and the JSON codec.
//!
//! I/O-free by design. The room state machine consumes [`ClientPacket`] and
//! produces [`ServerPacket`] without ever touching a socket, which is what lets
//! the whole of the game logic be tested synchronously — and lets anyone write
//! a Rust Archipelago client against this crate without pulling in a server.
//!
//! ```
//! use pahoa_proto::{decode, encode, ClientPacket, ServerPacket};
//! use pahoa_proto::server::{LocationInfo};
//!
//! let packets = decode(r#"[{"cmd":"Sync"}]"#).unwrap();
//! assert_eq!(packets[0], ClientPacket::Sync);
//!
//! let out = encode(&[ServerPacket::LocationInfo(LocationInfo { locations: vec![] })]);
//! assert_eq!(out, r#"[{"cmd":"LocationInfo","locations":[]}]"#);
//! ```

pub mod client;
pub mod codec;
pub mod depth;
pub mod server;
pub mod types;

pub use client::ClientPacket;
pub use codec::{DecodeError, decode, encode};
pub use depth::{DepthError, MAX_DEPTH, check_depth};
pub use server::ServerPacket;
pub use types::{
    ClientStatus, Hint, HintStatus, ItemsHandling, JsonMessagePart, NetworkItem, NetworkPlayer,
    NetworkSlot, Permission, SlotType, Version,
};
