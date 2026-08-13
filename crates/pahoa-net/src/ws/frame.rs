//! RFC 6455 framing.
//!
//! Small and completely specified, which is why it is here rather than taken
//! from a crate: what pahoa actually needs is the ability to hand the socket a
//! **pre-built frame** — header and all — so one broadcast can be encoded once,
//! compressed once, and written verbatim to thousands of connections. Every
//! WebSocket library owns compression per connection, because every WebSocket
//! library is built for point-to-point traffic.
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-------+-+-------------+-------------------------------+
//! |F|R|R|R| opcode|M| Payload len |    Extended payload length    |
//! |I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
//! |N|V|V|V|       |S|             |   (if payload len==126/127)   |
//! | |1|2|3|       |K|             |                               |
//! +-+-+-+-+-------+-+-------------+ - - - - - - - - - - - - - - - +
//! |     Extended payload length continued, if payload len == 127  |
//! + - - - - - - - - - - - - - - - +-------------------------------+
//! |                               |Masking-key, if MASK set to 1  |
//! +-------------------------------+-------------------------------+
//! ```
//!
//! `RSV1` is the permessage-deflate flag (RFC 7692 §6): set on the **first**
//! frame of a compressed message and on no other. `RSV2`/`RSV3` have no
//! negotiated meaning here and must be zero.

use bytes::{BufMut, Bytes, BytesMut};

/// Largest header pahoa ever writes: 2 bytes plus a 64-bit length. Server
/// frames are never masked, so no key.
const MAX_SERVER_HEADER: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl OpCode {
    fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0x0 => Self::Continuation,
            0x1 => Self::Text,
            0x2 => Self::Binary,
            0x8 => Self::Close,
            0x9 => Self::Ping,
            0xA => Self::Pong,
            // 0x3-0x7 and 0xB-0xF are reserved. Receiving one is a protocol
            // error rather than something to skip (RFC 6455 §5.2).
            _ => return None,
        })
    }

    fn bits(self) -> u8 {
        match self {
            Self::Continuation => 0x0,
            Self::Text => 0x1,
            Self::Binary => 0x2,
            Self::Close => 0x8,
            Self::Ping => 0x9,
            Self::Pong => 0xA,
        }
    }

    /// Control frames may not be fragmented and carry at most 125 bytes
    /// (RFC 6455 §5.5).
    pub fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    /// permessage-deflate: this message's payload is compressed.
    pub rsv1: bool,
    pub opcode: OpCode,
    pub payload: Bytes,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("reserved opcode {0:#x}")]
    ReservedOpcode(u8),
    #[error("RSV2 or RSV3 set, but no extension defines them")]
    ReservedBits,
    #[error("client frame was not masked")]
    Unmasked,
    #[error("control frame carried {0} bytes, the limit is 125")]
    ControlTooLarge(usize),
    #[error("control frame was fragmented")]
    FragmentedControl,
    #[error("frame of {len} bytes exceeds the {limit}-byte limit")]
    TooLarge { len: u64, limit: usize },
}

/// How much of a header is present, and what it says.
struct Header {
    fin: bool,
    rsv1: bool,
    opcode: OpCode,
    mask: Option<[u8; 4]>,
    payload_len: usize,
    /// Bytes consumed by the header itself.
    len: usize,
}

/// Parse a header from the front of `buf`.
///
/// `Ok(None)` means "not enough bytes yet", which is the normal case on a
/// stream and not an error.
fn parse_header(buf: &[u8], max_payload: usize) -> Result<Option<Header>, FrameError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let first = buf[0];
    let second = buf[1];

    let fin = first & 0x80 != 0;
    let rsv1 = first & 0x40 != 0;
    if first & 0x30 != 0 {
        return Err(FrameError::ReservedBits);
    }
    let opcode = OpCode::from_bits(first & 0x0F).ok_or(FrameError::ReservedOpcode(first & 0x0F))?;

    // A server must close on an unmasked client frame (RFC 6455 §5.1). This is
    // not pedantry: an unmasked stream from a browser is a cache-poisoning
    // vector, which is the whole reason masking exists.
    let masked = second & 0x80 != 0;
    if !masked {
        return Err(FrameError::Unmasked);
    }

    let short_len = (second & 0x7F) as usize;
    let (payload_len, len_bytes) = match short_len {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as u64, 2)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            (u64::from_be_bytes(buf[2..10].try_into().unwrap()), 8)
        }
        n => (n as u64, 0),
    };

    // Checked against the cap *before* it is used for anything, so a frame
    // claiming 2^63 bytes is refused rather than reserved for.
    if payload_len > max_payload as u64 {
        return Err(FrameError::TooLarge {
            len: payload_len,
            limit: max_payload,
        });
    }
    let payload_len = payload_len as usize;

    if opcode.is_control() {
        if !fin {
            return Err(FrameError::FragmentedControl);
        }
        if payload_len > 125 {
            return Err(FrameError::ControlTooLarge(payload_len));
        }
    }

    let mask_offset = 2 + len_bytes;
    if buf.len() < mask_offset + 4 {
        return Ok(None);
    }
    let mask = Some(buf[mask_offset..mask_offset + 4].try_into().unwrap());

    Ok(Some(Header {
        fin,
        rsv1,
        opcode,
        mask,
        payload_len,
        len: mask_offset + 4,
    }))
}

/// Take one frame off the front of `buf`, if a whole one is there.
///
/// Consumes exactly the bytes of that frame and leaves the rest, so this can be
/// driven straight from a read buffer.
pub fn decode(buf: &mut BytesMut, max_payload: usize) -> Result<Option<Frame>, FrameError> {
    let Some(header) = parse_header(buf, max_payload)? else {
        return Ok(None);
    };
    if buf.len() < header.len + header.payload_len {
        return Ok(None);
    }

    let _ = buf.split_to(header.len);
    let mut payload = buf.split_to(header.payload_len);
    if let Some(key) = header.mask {
        unmask(&mut payload, key);
    }

    Ok(Some(Frame {
        fin: header.fin,
        rsv1: header.rsv1,
        opcode: header.opcode,
        payload: payload.freeze(),
    }))
}

/// XOR with the repeating 4-byte key (RFC 6455 §5.3).
///
/// Word-at-a-time rather than byte-at-a-time: this runs over every inbound
/// byte, and a `Set` from a tracker can be a megabyte.
fn unmask(payload: &mut [u8], key: [u8; 4]) {
    let wide = u32::from_ne_bytes(key);
    let (chunks, tail) = payload.as_chunks_mut::<4>();
    for chunk in chunks {
        *chunk = (u32::from_ne_bytes(*chunk) ^ wide).to_ne_bytes();
    }
    for (byte, k) in tail.iter_mut().zip(key) {
        *byte ^= k;
    }
}

/// Build a complete server frame — header and payload — ready to write.
///
/// This is the whole point of owning this layer. The actor calls it once per
/// broadcast and the resulting `Bytes` is cloned to every recipient, so 6000
/// connections cost 6000 refcount bumps rather than 6000 framings.
pub fn build(opcode: OpCode, rsv1: bool, payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(MAX_SERVER_HEADER + payload.len());
    put_header(&mut out, opcode, rsv1, true, payload.len());
    out.put_slice(payload);
    out.freeze()
}

fn put_header(out: &mut BytesMut, opcode: OpCode, rsv1: bool, fin: bool, len: usize) {
    let mut first = opcode.bits();
    if fin {
        first |= 0x80;
    }
    if rsv1 {
        first |= 0x40;
    }
    out.put_u8(first);

    // Server frames are never masked, so the mask bit stays clear.
    if len < 126 {
        out.put_u8(len as u8);
    } else if len <= u16::MAX as usize {
        out.put_u8(126);
        out.put_u16(len as u16);
    } else {
        out.put_u8(127);
        out.put_u64(len as u64);
    }
}

/// A close frame carrying a status code and reason (RFC 6455 §5.5.1).
pub fn close(code: u16, reason: &str) -> Bytes {
    // 125-byte control limit, minus the two-byte code. Truncating on a char
    // boundary keeps the reason valid UTF-8, which the peer is entitled to.
    let mut cut = reason.len().min(123);
    while cut > 0 && !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut payload = Vec::with_capacity(2 + cut);
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(&reason.as_bytes()[..cut]);
    build(OpCode::Close, false, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame a payload the way a *client* does: masked, with a known key.
    fn client_frame(opcode: u8, fin: bool, rsv1: bool, payload: &[u8]) -> BytesMut {
        let key = [0xAA, 0xBB, 0xCC, 0xDD];
        let mut out = BytesMut::new();
        let mut first = opcode;
        if fin {
            first |= 0x80;
        }
        if rsv1 {
            first |= 0x40;
        }
        out.put_u8(first);
        let len = payload.len();
        if len < 126 {
            out.put_u8(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            out.put_u8(0x80 | 126);
            out.put_u16(len as u16);
        } else {
            out.put_u8(0x80 | 127);
            out.put_u64(len as u64);
        }
        out.put_slice(&key);
        out.put_slice(&masked(payload, key));
        out
    }

    fn masked(payload: &[u8], key: [u8; 4]) -> Vec<u8> {
        payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 4])
            .collect()
    }

    #[test]
    fn a_masked_text_frame_round_trips() {
        let mut buf = client_frame(0x1, true, false, b"hello");
        let frame = decode(&mut buf, 1 << 20).unwrap().expect("a whole frame");
        assert_eq!(frame.opcode, OpCode::Text);
        assert!(frame.fin);
        assert!(!frame.rsv1);
        assert_eq!(&frame.payload[..], b"hello");
        assert!(buf.is_empty(), "the frame should be fully consumed");
    }

    #[test]
    fn each_length_encoding_works() {
        // The three payload-length forms, at their boundaries.
        for len in [0usize, 1, 125, 126, 127, 65535, 65536] {
            let payload = vec![b'x'; len];
            let mut buf = client_frame(0x2, true, false, &payload);
            let frame = decode(&mut buf, 1 << 20).unwrap().expect("a whole frame");
            assert_eq!(frame.payload.len(), len, "len {len}");
            assert!(buf.is_empty(), "len {len} left trailing bytes");
        }
    }

    #[test]
    fn a_partial_frame_is_not_an_error() {
        // The normal case on a stream: ask again when more arrives.
        let whole = client_frame(0x1, true, false, b"hello world");
        for cut in 0..whole.len() {
            let mut buf = BytesMut::from(&whole[..cut]);
            assert_eq!(
                decode(&mut buf, 1 << 20),
                Ok(None),
                "{cut} bytes should be incomplete, not an error"
            );
            assert_eq!(buf.len(), cut, "an incomplete read must consume nothing");
        }
    }

    #[test]
    fn several_frames_in_one_buffer_come_out_one_at_a_time() {
        let mut buf = client_frame(0x1, true, false, b"one");
        buf.extend_from_slice(&client_frame(0x1, true, false, b"two"));
        buf.extend_from_slice(&client_frame(0x9, true, false, b""));

        assert_eq!(
            &decode(&mut buf, 1 << 20).unwrap().unwrap().payload[..],
            b"one"
        );
        assert_eq!(
            &decode(&mut buf, 1 << 20).unwrap().unwrap().payload[..],
            b"two"
        );
        assert_eq!(
            decode(&mut buf, 1 << 20).unwrap().unwrap().opcode,
            OpCode::Ping
        );
        assert_eq!(decode(&mut buf, 1 << 20), Ok(None));
    }

    #[test]
    fn an_unmasked_client_frame_is_refused() {
        // Masking is a cache-poisoning defense, not a formality, so this is a
        // hard error rather than something to tolerate.
        let mut buf = BytesMut::from(&[0x81u8, 0x05, b'h', b'e', b'l', b'l', b'o'][..]);
        assert_eq!(decode(&mut buf, 1 << 20), Err(FrameError::Unmasked));
    }

    #[test]
    fn reserved_opcodes_and_bits_are_refused() {
        let mut buf = client_frame(0x3, true, false, b"");
        assert_eq!(
            decode(&mut buf, 1 << 20),
            Err(FrameError::ReservedOpcode(0x3))
        );

        // RSV2 set, which no negotiated extension defines.
        let mut buf = BytesMut::from(&[0xA1u8, 0x80, 0, 0, 0, 0][..]);
        assert_eq!(decode(&mut buf, 1 << 20), Err(FrameError::ReservedBits));
    }

    #[test]
    fn control_frames_must_be_short_and_unfragmented() {
        let mut buf = client_frame(0x9, true, false, &[b'x'; 126]);
        assert_eq!(
            decode(&mut buf, 1 << 20),
            Err(FrameError::ControlTooLarge(126))
        );

        let mut buf = client_frame(0x8, false, false, b"");
        assert_eq!(
            decode(&mut buf, 1 << 20),
            Err(FrameError::FragmentedControl)
        );
    }

    #[test]
    fn an_oversized_length_is_refused_before_it_is_believed() {
        // The header claims 2^40 bytes. Nothing should try to hold that.
        let mut buf = BytesMut::new();
        buf.put_u8(0x82);
        buf.put_u8(0x80 | 127);
        buf.put_u64(1 << 40);
        buf.put_slice(&[0, 0, 0, 0]);
        assert_eq!(
            decode(&mut buf, 1 << 20),
            Err(FrameError::TooLarge {
                len: 1 << 40,
                limit: 1 << 20
            })
        );
    }

    #[test]
    fn built_frames_are_unmasked_and_carry_the_right_bits() {
        let frame = build(OpCode::Text, true, b"hi");
        // FIN | RSV1 | Text, then an unmasked length of 2.
        assert_eq!(&frame[..], &[0xC1, 0x02, b'h', b'i']);

        let long = build(OpCode::Binary, false, &[0u8; 200]);
        assert_eq!(long[0], 0x82);
        assert_eq!(long[1], 126, "200 bytes needs the 16-bit length form");
        assert_eq!(u16::from_be_bytes([long[2], long[3]]), 200);
    }

    #[test]
    fn unmasking_handles_lengths_that_are_not_a_multiple_of_four() {
        // The word-at-a-time path plus every possible tail length.
        for len in 0..24 {
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut buf = client_frame(0x2, true, false, &payload);
            let frame = decode(&mut buf, 1 << 20).unwrap().unwrap();
            assert_eq!(&frame.payload[..], &payload[..], "len {len}");
        }
    }

    #[test]
    fn a_close_reason_is_truncated_on_a_character_boundary() {
        // 125-byte control limit. Cutting mid-character would hand the peer
        // invalid UTF-8 in a frame it is entitled to read as text.
        let long = "é".repeat(100);
        let frame = close(1000, &long);
        let payload = &frame[2..];
        assert!(payload.len() <= 125);
        assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1000);
        std::str::from_utf8(&payload[2..]).expect("the reason stays valid UTF-8");
    }
}
