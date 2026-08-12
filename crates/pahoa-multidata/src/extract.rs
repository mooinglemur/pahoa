//! Path-tracking accessors over [`PyObj`].
//!
//! Every extraction carries a [`Path`] so a shape change in a future
//! Archipelago release reports as `slot_info[3].game: expected str, found int`
//! rather than a bare type error a hundred lines deep.

use crate::error::{Error, Path, Result};
use pahoa_pickle::PyObj;

pub trait Extract {
    fn at(&self, path: &Path, key: &str) -> Result<&PyObj>;
    fn opt(&self, key: &str) -> Option<&PyObj>;
    fn str_(&self, path: &Path) -> Result<&str>;
    fn int(&self, path: &Path) -> Result<i64>;
    fn bool_(&self, path: &Path) -> Result<bool>;
    fn seq(&self, path: &Path) -> Result<&[PyObj]>;
    fn dict_(&self, path: &Path) -> Result<&[(PyObj, PyObj)]>;
    fn tuple_n(&self, path: &Path, n: usize) -> Result<&[PyObj]>;

    /// An integer narrowed to `u32`, for slot ids and similar.
    fn u32_(&self, path: &Path) -> Result<u32> {
        let v = self.int(path)?;
        u32::try_from(v).map_err(|_| Error::Range {
            path: path.clone(),
            value: v,
            target: "u32",
        })
    }
}

impl Extract for PyObj {
    fn at(&self, path: &Path, key: &str) -> Result<&PyObj> {
        self.get(key).ok_or_else(|| Error::Missing {
            path: path.key(key),
        })
    }

    fn opt(&self, key: &str) -> Option<&PyObj> {
        self.get(key)
    }

    fn str_(&self, path: &Path) -> Result<&str> {
        self.as_str().ok_or_else(|| Error::Type {
            path: path.clone(),
            expected: "str",
            found: self.type_name(),
        })
    }

    fn int(&self, path: &Path) -> Result<i64> {
        self.as_int().ok_or_else(|| Error::Type {
            path: path.clone(),
            expected: "int",
            found: self.type_name(),
        })
    }

    fn bool_(&self, path: &Path) -> Result<bool> {
        // Python treats 0/1 and False/True interchangeably here, and multidata
        // has historically carried both for the same field.
        match self {
            PyObj::Bool(b) => Ok(*b),
            PyObj::Int(0) => Ok(false),
            PyObj::Int(1) => Ok(true),
            other => Err(Error::Type {
                path: path.clone(),
                expected: "bool",
                found: other.type_name(),
            }),
        }
    }

    fn seq(&self, path: &Path) -> Result<&[PyObj]> {
        self.as_seq().ok_or_else(|| Error::Type {
            path: path.clone(),
            expected: "list, tuple or set",
            found: self.type_name(),
        })
    }

    fn dict_(&self, path: &Path) -> Result<&[(PyObj, PyObj)]> {
        self.as_dict().ok_or_else(|| Error::Type {
            path: path.clone(),
            expected: "dict",
            found: self.type_name(),
        })
    }

    fn tuple_n(&self, path: &Path, n: usize) -> Result<&[PyObj]> {
        let items = self.seq(path)?;
        if items.len() != n {
            return Err(Error::Arity {
                path: path.clone(),
                expected: n,
                found: items.len(),
            });
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_path_of_a_type_error() {
        let v = PyObj::Int(3);
        let err = v
            .str_(&Path::root().key("slot_info").index(3).key("game"))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "slot_info[3].game: expected str, found int"
        );
    }

    #[test]
    fn reports_missing_keys_with_their_path() {
        let v = PyObj::Dict(vec![]);
        let err = v.at(&Path::root(), "seed_name").unwrap_err();
        assert_eq!(err.to_string(), "seed_name: missing required key");
    }

    #[test]
    fn accepts_python_int_bools() {
        let p = Path::root();
        assert!(!PyObj::Int(0).bool_(&p).unwrap());
        assert!(PyObj::Int(1).bool_(&p).unwrap());
        assert!(PyObj::Bool(true).bool_(&p).unwrap());
        assert!(PyObj::Int(2).bool_(&p).is_err());
    }

    #[test]
    fn arity_errors_name_both_counts() {
        let v = PyObj::Tuple(vec![PyObj::Int(1), PyObj::Int(2)]);
        let err = v.tuple_n(&Path::root().key("locations"), 3).unwrap_err();
        assert_eq!(
            err.to_string(),
            "locations: expected 3, found a 2-element tuple"
        );
    }
}
