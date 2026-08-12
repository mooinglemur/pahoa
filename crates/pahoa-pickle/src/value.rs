//! The Python object model, restricted to what Archipelago actually pickles.

use crate::bigint::BigInt;
use std::fmt;

/// A dotted `module.name` reference produced by `STACK_GLOBAL`.
///
/// Keeping class identity is the whole reason this crate exists rather than
/// using `serde-pickle`, which discards it and decodes every class-typed object
/// to a bare tuple. Retaining it buys real error messages ("expected
/// `NetUtils.Hint`, got `NetUtils.NetworkItem`") and lets us detect multidata
/// shape changes across Archipelago releases instead of mis-parsing positionally.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ClassId {
    pub module: Box<str>,
    pub name: Box<str>,
}

impl ClassId {
    pub fn new(module: impl Into<Box<str>>, name: impl Into<Box<str>>) -> Self {
        Self {
            module: module.into(),
            name: name.into(),
        }
    }

    pub fn matches(&self, module: &str, name: &str) -> bool {
        &*self.module == module && &*self.name == name
    }
}

impl fmt::Display for ClassId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.module, self.name)
    }
}

impl fmt::Debug for ClassId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// A decoded Python value.
///
/// `Dict` is a `Vec` of pairs rather than a map because Python dicts preserve
/// insertion order and that order is observable downstream (see the `preserve_order`
/// discussion for datastorage). It also sidesteps needing `PyObj` to be `Hash`/`Eq`
/// under Python's equality rules, where `1 == 1.0 == True`.
#[derive(Clone, PartialEq)]
pub enum PyObj {
    None,
    Bool(bool),
    Int(i64),
    /// A Python int too large for `i64`. Rare but real — see [`crate::BigInt`].
    /// The reader narrows to `Int` whenever it fits, so a given value has
    /// exactly one representation and equality stays meaningful.
    Big(BigInt),
    Float(f64),
    Str(Box<str>),
    List(Vec<PyObj>),
    Tuple(Vec<PyObj>),
    Dict(Vec<(PyObj, PyObj)>),
    Set(Vec<PyObj>),
    /// A class reference sitting on the stack, awaiting `REDUCE`/`NEWOBJ`.
    /// Transient in every stream we care about, but representable.
    Global(ClassId),
    /// `cls(*args)` (via `REDUCE`) or `cls.__new__(cls, *args)` (via `NEWOBJ`).
    /// Archipelago uses the former for by-value enums and the latter for namedtuples;
    /// both collapse to the same shape here.
    Instance {
        class: ClassId,
        args: Vec<PyObj>,
    },
}

impl PyObj {
    pub fn type_name(&self) -> &'static str {
        match self {
            PyObj::None => "None",
            PyObj::Bool(_) => "bool",
            PyObj::Int(_) | PyObj::Big(_) => "int",
            PyObj::Float(_) => "float",
            PyObj::Str(_) => "str",
            PyObj::List(_) => "list",
            PyObj::Tuple(_) => "tuple",
            PyObj::Dict(_) => "dict",
            PyObj::Set(_) => "set",
            PyObj::Global(_) => "global",
            PyObj::Instance { .. } => "instance",
        }
    }

    /// `None` for a [`PyObj::Big`], which by construction does not fit.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            PyObj::Int(i) => Some(*i),
            // Python bools are ints, and the distinction is invisible to callers
            // that want a number.
            PyObj::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_big(&self) -> Option<&BigInt> {
        match self {
            PyObj::Big(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            PyObj::Bool(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match *self {
            PyObj::Float(f) => Some(f),
            PyObj::Int(i) => Some(i as f64),
            _ => None,
        }
    }

    /// True for any integer, wide or not. Use where the width is irrelevant.
    pub fn is_int(&self) -> bool {
        matches!(self, PyObj::Int(_) | PyObj::Big(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            PyObj::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Sequence view over `list`, `tuple`, or `set`.
    pub fn as_seq(&self) -> Option<&[PyObj]> {
        match self {
            PyObj::List(v) | PyObj::Tuple(v) | PyObj::Set(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&[(PyObj, PyObj)]> {
        match self {
            PyObj::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Positional arguments of an instance of exactly `module.name`.
    pub fn as_instance_of(&self, module: &str, name: &str) -> Option<&[PyObj]> {
        match self {
            PyObj::Instance { class, args } if class.matches(module, name) => Some(args),
            _ => None,
        }
    }

    /// Look up a string key in a dict. Linear, which is fine: multidata's
    /// top-level dict has 16 keys and this is used for structural access, not
    /// in any hot path.
    pub fn get(&self, key: &str) -> Option<&PyObj> {
        self.as_dict()?
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    }
}

impl fmt::Debug for PyObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PyObj::None => write!(f, "None"),
            PyObj::Bool(b) => write!(f, "{b}"),
            PyObj::Int(i) => write!(f, "{i}"),
            PyObj::Big(b) => write!(f, "{b}"),
            PyObj::Float(x) => write!(f, "{x}"),
            PyObj::Str(s) => write!(f, "{s:?}"),
            PyObj::List(v) => f.debug_list().entries(v).finish(),
            PyObj::Tuple(v) => {
                write!(f, "(")?;
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item:?}")?;
                }
                if v.len() == 1 {
                    write!(f, ",")?;
                }
                write!(f, ")")
            }
            PyObj::Dict(d) => {
                let mut m = f.debug_map();
                for (k, v) in d {
                    m.entry(k, v);
                }
                m.finish()
            }
            PyObj::Set(v) => f.debug_set().entries(v).finish(),
            PyObj::Global(c) => write!(f, "<{c}>"),
            PyObj::Instance { class, args } => {
                write!(f, "{class}")?;
                f.debug_list().entries(args).finish()
            }
        }
    }
}
