use thiserror::Error;

/// Where in the multidata a problem occurred, e.g. `slot_info[3].game`.
///
/// Multidata shape drifts between Archipelago releases, so "expected a string"
/// is nearly useless on its own — the whole value of a typed loader is saying
/// *which* field moved. Paths are built as the loader descends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn key(&self, k: &str) -> Self {
        if self.0.is_empty() {
            Self(k.to_string())
        } else {
            Self(format!("{}.{}", self.0, k))
        }
    }

    pub fn index(&self, i: impl std::fmt::Display) -> Self {
        Self(format!("{}[{}]", self.0, i))
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("<root>")
        } else {
            f.write_str(&self.0)
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("multidata is empty")]
    Empty,

    #[error("unsupported multidata format version {0} (this server understands up to 3)")]
    UnsupportedFormat(u8),

    #[error("could not decompress multidata: {0}")]
    Decompress(#[from] std::io::Error),

    #[error("could not decode multidata pickle: {0}")]
    Pickle(#[from] pahoa_pickle::Error),

    #[error("{path}: missing required key")]
    Missing { path: Path },

    #[error("{path}: expected {expected}, found {found}")]
    Type {
        path: Path,
        expected: &'static str,
        found: &'static str,
    },

    #[error("{path}: expected {expected}, found a {found}-element tuple")]
    Arity {
        path: Path,
        expected: usize,
        found: usize,
    },

    #[error("{path}: value {value} is out of range for {target}")]
    Range {
        path: Path,
        value: i64,
        target: &'static str,
    },

    #[error("{path}: {value} is not a valid {name}")]
    Enum {
        path: Path,
        name: &'static str,
        value: i64,
    },

    /// The reference server validates the location table on load and refuses to
    /// host an inconsistent one; so do we, rather than discovering it mid-game.
    #[error("locations table is invalid: {0}")]
    Locations(String),

    #[error("data package for {game:?} is invalid: {reason}")]
    DataPackage { game: String, reason: String },

    #[error("could not read data package snapshot: {0}")]
    Snapshot(String),
}

pub type Result<T> = std::result::Result<T, Error>;
