//! Archipelago data storage: the `Set` operation semantics.
//!
//! Clients use data storage for DeathLink, trackers and per-world state, and
//! the operations are Python expressions over arbitrary JSON. Reproducing them
//! means reproducing a slice of CPython — see [`ops`] for the deliberate
//! divergences and [`pyvalue`] for the type traps underneath.
//!
//! ```
//! use pahoa_datastore::apply;
//! use serde_json::json;
//!
//! // Python's `%` follows the divisor's sign, unlike Rust's.
//! assert_eq!(apply("mod", json!(-7), &json!(3)).unwrap(), json!(2));
//! // `+` concatenates strings and extends lists as well as adding numbers.
//! assert_eq!(apply("add", json!("ab"), &json!("c")).unwrap(), json!("abc"));
//! // Booleans are integers.
//! assert_eq!(apply("add", json!(true), &json!(1)).unwrap(), json!(2));
//! ```

pub mod ops;
pub mod pyvalue;

pub use ops::{MAX_RESULT_LEN, OpError, apply, apply_all};
