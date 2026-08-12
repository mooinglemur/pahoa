//! A streaming interpreter for the pickle opcode subset Archipelago emits.
//!
//! Scope is deliberately closed. Decoding four real `.archipelago` files
//! (75/35/17630 slots/games/locations being the largest) yields a union of
//! exactly 33 opcodes, all protocol 4. Anything outside that set is refused
//! with [`Error::UnsupportedOpcode`] rather than handled generically — a
//! narrow, auditable reader beats a permissive one for a format that is both
//! attacker-influenced and a remote-code-execution vector in its native runtime.

use crate::allowlist::Allowlist;
use crate::bigint::BigInt;
use crate::error::{Error, Result};
use crate::value::{ClassId, PyObj};

// Opcodes we implement.
mod op {
    pub const MARK: u8 = b'(';
    pub const STOP: u8 = b'.';
    pub const NONE: u8 = b'N';
    pub const BININT: u8 = b'J';
    pub const BININT1: u8 = b'K';
    pub const BININT2: u8 = b'M';
    pub const BINFLOAT: u8 = b'G';
    pub const BINUNICODE: u8 = b'X';
    pub const EMPTY_LIST: u8 = b']';
    pub const APPEND: u8 = b'a';
    pub const APPENDS: u8 = b'e';
    pub const EMPTY_DICT: u8 = b'}';
    pub const SETITEM: u8 = b's';
    pub const SETITEMS: u8 = b'u';
    pub const EMPTY_TUPLE: u8 = b')';
    pub const TUPLE: u8 = b't';
    pub const BINGET: u8 = b'h';
    pub const LONG_BINGET: u8 = b'j';
    pub const REDUCE: u8 = b'R';
    pub const PROTO: u8 = 0x80;
    pub const NEWOBJ: u8 = 0x81;
    pub const TUPLE1: u8 = 0x85;
    pub const TUPLE2: u8 = 0x86;
    pub const TUPLE3: u8 = 0x87;
    pub const NEWTRUE: u8 = 0x88;
    pub const NEWFALSE: u8 = 0x89;
    pub const LONG1: u8 = 0x8a;
    pub const SHORT_BINUNICODE: u8 = 0x8c;
    pub const EMPTY_SET: u8 = 0x8f;
    pub const ADDITEMS: u8 = 0x90;
    pub const STACK_GLOBAL: u8 = 0x93;
    pub const MEMOIZE: u8 = 0x94;
    pub const FRAME: u8 = 0x95;
}

/// Valid pickle opcodes we deliberately do not implement, named so the error
/// says which one rather than just a byte. Everything here is either legacy
/// text-protocol, an opcode Archipelago has never been observed to emit, or —
/// in the case of the `*_GLOBAL`/`INST` family — a construct we refuse on
/// principle because it names arbitrary importable objects.
fn known_unsupported(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        b'0' => "POP",
        b'1' => "POP_MARK",
        b'2' => "DUP",
        b'F' => "FLOAT",
        b'I' => "INT",
        b'L' => "LONG",
        b'S' => "STRING",
        b'T' => "BINSTRING",
        b'U' => "SHORT_BINSTRING",
        b'V' => "UNICODE",
        b'c' => "GLOBAL",
        b'd' => "DICT",
        b'g' => "GET",
        b'i' => "INST",
        b'l' => "LIST",
        b'o' => "OBJ",
        b'p' => "PUT",
        b'q' => "BINPUT",
        b'r' => "LONG_BINPUT",
        b'b' => "BUILD",
        b'P' => "PERSID",
        b'Q' => "BINPERSID",
        0x82 => "EXT1",
        0x83 => "EXT2",
        0x84 => "EXT4",
        0x8b => "LONG4",
        0x8d => "BINBYTES8",
        0x8e => "BYTEARRAY8",
        0x91 => "FROZENSET",
        0x92 => "NEWOBJ_EX",
        0x96 => "BYTEARRAY8",
        0x97 => "NEXT_BUFFER",
        0x98 => "READONLY_BUFFER",
        _ => return None,
    })
}

/// Guards against a hostile stream building unbounded nesting. Real multidata
/// nests ~6 deep; 256 is far above anything legitimate and far below anything
/// that would exhaust memory.
const MAX_DEPTH: usize = 256;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    stack: Vec<PyObj>,
    marks: Vec<usize>,
    memo: Vec<PyObj>,
    allowlist: &'a Allowlist,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], allowlist: &'a Allowlist) -> Self {
        Self {
            buf,
            pos: 0,
            stack: Vec::new(),
            marks: Vec::new(),
            memo: Vec::new(),
            allowlist,
        }
    }

    // --- primitive reads -------------------------------------------------

    fn need(&self, n: usize) -> Result<()> {
        if self.buf.len() - self.pos < n {
            return Err(Error::Truncated {
                offset: self.pos,
                wanted: n,
                available: self.buf.len() - self.pos,
            });
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16le(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i32le(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u32le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64le(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn utf8(&mut self, len: usize) -> Result<Box<str>> {
        let at = self.pos;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(|s| s.into())
            .map_err(|_| Error::BadUtf8 { offset: at })
    }

    // --- stack helpers ---------------------------------------------------

    fn push(&mut self, v: PyObj) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH * 4096 {
            return Err(Error::TooDeep {
                offset: self.pos,
                limit: MAX_DEPTH,
            });
        }
        self.stack.push(v);
        Ok(())
    }

    fn pop(&mut self, opcode: &'static str) -> Result<PyObj> {
        // A mark still on the stack means the stream popped past its own frame.
        if self.marks.last().is_some_and(|&m| m >= self.stack.len()) {
            return Err(Error::StackUnderflow {
                offset: self.pos,
                opcode,
            });
        }
        self.stack.pop().ok_or(Error::StackUnderflow {
            offset: self.pos,
            opcode,
        })
    }

    /// Pop everything above the most recent mark, consuming the mark.
    fn pop_to_mark(&mut self, opcode: &'static str) -> Result<Vec<PyObj>> {
        let m = self.marks.pop().ok_or(Error::NoMark {
            offset: self.pos,
            opcode,
        })?;
        if m > self.stack.len() {
            return Err(Error::StackUnderflow {
                offset: self.pos,
                opcode,
            });
        }
        Ok(self.stack.split_off(m))
    }

    fn memo_get(&self, index: usize) -> Result<PyObj> {
        self.memo.get(index).cloned().ok_or(Error::BadMemo {
            offset: self.pos,
            index,
            len: self.memo.len(),
        })
    }

    // --- main loop -------------------------------------------------------

    pub fn decode(mut self) -> Result<PyObj> {
        loop {
            let at = self.pos;
            let opcode = self.u8()?;
            match opcode {
                op::PROTO => {
                    let v = self.u8()?;
                    // Archipelago writes protocol 4. Accept 2..=5 so a generator
                    // bump doesn't hard-fail before we've looked at it.
                    if !(2..=5).contains(&v) {
                        return Err(Error::UnsupportedProtocol {
                            offset: at,
                            version: v,
                        });
                    }
                }
                // A framing hint only; the payload follows inline.
                op::FRAME => {
                    self.u64le()?;
                }

                op::NONE => self.push(PyObj::None)?,
                op::NEWTRUE => self.push(PyObj::Bool(true))?,
                op::NEWFALSE => self.push(PyObj::Bool(false))?,

                op::BININT => {
                    let v = self.i32le()?;
                    self.push(PyObj::Int(v as i64))?;
                }
                op::BININT1 => {
                    let v = self.u8()?;
                    self.push(PyObj::Int(v as i64))?;
                }
                op::BININT2 => {
                    let v = self.u16le()?;
                    self.push(PyObj::Int(v as i64))?;
                }
                op::LONG1 => {
                    let n = self.u8()? as usize;
                    let bytes = self.take(n)?;
                    self.push(long_from_le(bytes))?;
                }
                op::BINFLOAT => {
                    // Note: big-endian, unlike every other numeric opcode.
                    let v = f64::from_be_bytes(self.take(8)?.try_into().unwrap());
                    self.push(PyObj::Float(v))?;
                }

                op::SHORT_BINUNICODE => {
                    let n = self.u8()? as usize;
                    let s = self.utf8(n)?;
                    self.push(PyObj::Str(s))?;
                }
                op::BINUNICODE => {
                    let n = self.u32le()? as usize;
                    let s = self.utf8(n)?;
                    self.push(PyObj::Str(s))?;
                }

                op::MARK => self.marks.push(self.stack.len()),

                op::EMPTY_TUPLE => self.push(PyObj::Tuple(Vec::new()))?,
                op::TUPLE1 => {
                    let a = self.pop("TUPLE1")?;
                    self.push(PyObj::Tuple(vec![a]))?;
                }
                op::TUPLE2 => {
                    let b = self.pop("TUPLE2")?;
                    let a = self.pop("TUPLE2")?;
                    self.push(PyObj::Tuple(vec![a, b]))?;
                }
                op::TUPLE3 => {
                    let c = self.pop("TUPLE3")?;
                    let b = self.pop("TUPLE3")?;
                    let a = self.pop("TUPLE3")?;
                    self.push(PyObj::Tuple(vec![a, b, c]))?;
                }
                op::TUPLE => {
                    let items = self.pop_to_mark("TUPLE")?;
                    self.push(PyObj::Tuple(items))?;
                }

                op::EMPTY_LIST => self.push(PyObj::List(Vec::new()))?,
                op::APPEND => {
                    let v = self.pop("APPEND")?;
                    match self.stack.last_mut() {
                        Some(PyObj::List(l)) => l.push(v),
                        other => {
                            return Err(Error::TypeMismatch {
                                offset: at,
                                opcode: "APPEND",
                                expected: "list",
                                found: other.map_or("nothing", |o| o.type_name()),
                            });
                        }
                    }
                }
                op::APPENDS => {
                    let items = self.pop_to_mark("APPENDS")?;
                    match self.stack.last_mut() {
                        Some(PyObj::List(l)) => l.extend(items),
                        other => {
                            return Err(Error::TypeMismatch {
                                offset: at,
                                opcode: "APPENDS",
                                expected: "list",
                                found: other.map_or("nothing", |o| o.type_name()),
                            });
                        }
                    }
                }

                op::EMPTY_SET => self.push(PyObj::Set(Vec::new()))?,
                op::ADDITEMS => {
                    let items = self.pop_to_mark("ADDITEMS")?;
                    match self.stack.last_mut() {
                        Some(PyObj::Set(s)) => s.extend(items),
                        other => {
                            return Err(Error::TypeMismatch {
                                offset: at,
                                opcode: "ADDITEMS",
                                expected: "set",
                                found: other.map_or("nothing", |o| o.type_name()),
                            });
                        }
                    }
                }

                op::EMPTY_DICT => self.push(PyObj::Dict(Vec::new()))?,
                op::SETITEM => {
                    let v = self.pop("SETITEM")?;
                    let k = self.pop("SETITEM")?;
                    match self.stack.last_mut() {
                        Some(PyObj::Dict(d)) => d.push((k, v)),
                        other => {
                            return Err(Error::TypeMismatch {
                                offset: at,
                                opcode: "SETITEM",
                                expected: "dict",
                                found: other.map_or("nothing", |o| o.type_name()),
                            });
                        }
                    }
                }
                op::SETITEMS => {
                    let items = self.pop_to_mark("SETITEMS")?;
                    if items.len() % 2 != 0 {
                        return Err(Error::OddSetItems { offset: at });
                    }
                    match self.stack.last_mut() {
                        Some(PyObj::Dict(d)) => {
                            let mut it = items.into_iter();
                            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                                d.push((k, v));
                            }
                        }
                        other => {
                            return Err(Error::TypeMismatch {
                                offset: at,
                                opcode: "SETITEMS",
                                expected: "dict",
                                found: other.map_or("nothing", |o| o.type_name()),
                            });
                        }
                    }
                }

                op::MEMOIZE => {
                    let top = self
                        .stack
                        .last()
                        .ok_or(Error::StackUnderflow {
                            offset: at,
                            opcode: "MEMOIZE",
                        })?
                        .clone();
                    self.memo.push(top);
                }
                op::BINGET => {
                    let i = self.u8()? as usize;
                    let v = self.memo_get(i)?;
                    self.push(v)?;
                }
                op::LONG_BINGET => {
                    let i = self.u32le()? as usize;
                    let v = self.memo_get(i)?;
                    self.push(v)?;
                }

                op::STACK_GLOBAL => {
                    let name = self.pop("STACK_GLOBAL")?;
                    let module = self.pop("STACK_GLOBAL")?;
                    let (PyObj::Str(module), PyObj::Str(name)) = (module, name) else {
                        return Err(Error::TypeMismatch {
                            offset: at,
                            opcode: "STACK_GLOBAL",
                            expected: "two strings",
                            found: "something else",
                        });
                    };
                    let class = ClassId { module, name };
                    if !self.allowlist.permits(&class) {
                        return Err(Error::ForbiddenClass { offset: at, class });
                    }
                    self.push(PyObj::Global(class))?;
                }

                // REDUCE is `cls(*args)`, NEWOBJ is `cls.__new__(cls, *args)`.
                // Archipelago uses the first for by-value enums and the second for
                // namedtuples; both land on the same representation here.
                op::REDUCE | op::NEWOBJ => {
                    let name = if opcode == op::REDUCE {
                        "REDUCE"
                    } else {
                        "NEWOBJ"
                    };
                    let args = self.pop(name)?;
                    let callable = self.pop(name)?;
                    let PyObj::Tuple(args) = args else {
                        return Err(Error::TypeMismatch {
                            offset: at,
                            opcode: name,
                            expected: "tuple of arguments",
                            found: args.type_name(),
                        });
                    };
                    let PyObj::Global(class) = callable else {
                        return Err(Error::TypeMismatch {
                            offset: at,
                            opcode: name,
                            expected: "class",
                            found: callable.type_name(),
                        });
                    };
                    self.push(PyObj::Instance { class, args })?;
                }

                op::STOP => {
                    if self.stack.len() != 1 {
                        return Err(Error::UnbalancedStack {
                            len: self.stack.len(),
                        });
                    }
                    return Ok(self.stack.pop().unwrap());
                }

                other => {
                    return Err(match known_unsupported(other) {
                        Some(name) => Error::UnsupportedOpcode {
                            offset: at,
                            opcode: other,
                            name,
                        },
                        None => Error::UnknownOpcode {
                            offset: at,
                            opcode: other,
                        },
                    });
                }
            }
        }
    }
}

/// LONG1 payload: little-endian two's complement, variable width, empty means 0.
///
/// Narrows to [`PyObj::Int`] whenever the value fits so that a given integer has
/// exactly one representation, and only widens to [`PyObj::Big`] when it must.
/// Oversized values are not an error: real multidata carries a `slot_data` field
/// larger than `u64`, and `slot_data` is forwarded to clients verbatim.
fn long_from_le(bytes: &[u8]) -> PyObj {
    let big = BigInt::from_le_twos_complement(bytes);
    match big.to_i64() {
        Some(v) => PyObj::Int(v),
        None => PyObj::Big(big),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(bytes: &[u8]) -> PyObj {
        long_from_le(bytes)
    }

    #[test]
    fn long1_decodes_signed_little_endian() {
        assert_eq!(long(&[]), PyObj::Int(0));
        assert_eq!(long(&[0x01]), PyObj::Int(1));
        assert_eq!(long(&[0xff]), PyObj::Int(-1));
        assert_eq!(long(&[0x00, 0x01]), PyObj::Int(256));
        assert_eq!(long(&[0x80]), PyObj::Int(-128));
        // 2**53, the Archipelago item/location id bound.
        assert_eq!(long(&[0, 0, 0, 0, 0, 0, 0x20, 0x00]), PyObj::Int(1 << 53));
    }

    #[test]
    fn long1_narrows_sign_extended_values() {
        // 9 bytes but semantically -1: must not become a Big.
        assert_eq!(long(&[0xff; 9]), PyObj::Int(-1));
    }

    #[test]
    fn long1_widens_only_when_necessary() {
        assert_eq!(long(&i64::MAX.to_le_bytes()), PyObj::Int(i64::MAX));

        // 2**64 exceeds i64 and must be preserved, not truncated.
        let bytes = [0, 0, 0, 0, 0, 0, 0, 0, 0x01];
        let v = long(&bytes);
        assert_eq!(
            v.as_big().map(ToString::to_string).as_deref(),
            Some("18446744073709551616")
        );
    }
}
