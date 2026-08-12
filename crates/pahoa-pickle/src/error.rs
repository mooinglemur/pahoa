use crate::value::ClassId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("truncated pickle: wanted {wanted} byte(s) at offset {offset}, {available} available")]
    Truncated {
        offset: usize,
        wanted: usize,
        available: usize,
    },

    /// Deliberately distinct from a malformed stream: an opcode outside the
    /// allowlist means the input is *valid* pickle we have chosen not to
    /// support, which is a different diagnosis and a different fix.
    #[error("unsupported opcode {opcode:#04x} ({name}) at offset {offset}")]
    UnsupportedOpcode {
        offset: usize,
        opcode: u8,
        name: &'static str,
    },

    #[error("unknown opcode {opcode:#04x} at offset {offset}")]
    UnknownOpcode { offset: usize, opcode: u8 },

    #[error("unsupported pickle protocol {version} (expected 2..=5) at offset {offset}")]
    UnsupportedProtocol { offset: usize, version: u8 },

    #[error("stack underflow during {opcode} at offset {offset}")]
    StackUnderflow { offset: usize, opcode: &'static str },

    #[error("no MARK on the stack during {opcode} at offset {offset}")]
    NoMark { offset: usize, opcode: &'static str },

    #[error("invalid memo index {index} at offset {offset} (memo holds {len})")]
    BadMemo {
        offset: usize,
        index: usize,
        len: usize,
    },

    #[error("invalid UTF-8 in string at offset {offset}")]
    BadUtf8 { offset: usize },

    #[error("integer at offset {offset} does not fit in i64 ({bytes} bytes)")]
    IntegerTooLarge { offset: usize, bytes: usize },

    #[error("{opcode} at offset {offset} expected {expected}, found {found}")]
    TypeMismatch {
        offset: usize,
        opcode: &'static str,
        expected: &'static str,
        found: &'static str,
    },

    /// The `restricted_loads` equivalent. Multidata and save files are
    /// attacker-influenced (datastorage holds arbitrary client-supplied values),
    /// so an unexpected class is refused rather than constructed.
    #[error("class {class} is not permitted at offset {offset}")]
    ForbiddenClass { offset: usize, class: ClassId },

    #[error("SETITEMS at offset {offset} got an odd number of stack items")]
    OddSetItems { offset: usize },

    #[error("pickle ended with {len} item(s) on the stack, expected exactly 1")]
    UnbalancedStack { len: usize },

    #[error("no STOP opcode found")]
    NoStop,

    #[error("nesting deeper than {limit} at offset {offset}")]
    TooDeep { offset: usize, limit: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
