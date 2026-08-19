//! A typed model of Archipelago's `.archipelago` multidata.
//!
//! Turns the untyped [`pahoa_pickle::PyObj`] tree into the structures the
//! server actually works with — slots, the location table, hints, versions, and
//! the merged data package — reporting shape problems with the path that
//! failed rather than a bare type error.
//!
//! ```no_run
//! use pahoa_multidata::MultiData;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let raw = std::fs::read("seed.archipelago")?;
//! let md = MultiData::parse(&raw)?;
//! println!("{} slots, {} locations", md.slot_info.len(), md.locations.len());
//! # Ok(())
//! # }
//! ```

mod datapackage;
mod error;
mod extract;
pub mod hint_blacklist;
mod locations;
mod multidata;
mod types;

pub use datapackage::{DataPackage, GameNames, GamePackage, MergeReport};
pub use error::{Error, Path, Result};
pub use locations::{LocationEntry, LocationStore};
pub use multidata::{MAX_FORMAT_VERSION, MIN_CLIENT_VERSION, MultiData};
pub use types::{
    ClientStatus, Hint, HintIdentity, HintStatus, NetworkItem, NetworkSlot, SlotType, Version,
    item_flags,
};
