//! A WebSocket layer built for broadcast.
//!
//! This exists rather than a dependency for one reason: pahoa needs to encode a
//! message once, compress it once, and write the resulting bytes verbatim to
//! thousands of connections. Every WebSocket library owns compression per
//! connection, because every WebSocket library is built for point-to-point
//! traffic — which turns one broadcast into 6000 compressions and is exactly
//! what `server_no_context_takeover` exists to avoid.
//!
//! The layers, bottom up:
//!
//! - [`frame`] — RFC 6455 framing: header parsing, masking, size caps
//! - [`deflate`] — RFC 7692 permessage-deflate, with the sync-flush trailer trick
//! - [`message`] — fragments to messages, with the inflate-then-validate ordering
//! - [`handshake`] / [`accept`] — the upgrade and what gets negotiated

pub mod accept;
pub mod deflate;
pub mod frame;
pub mod handshake;
pub mod message;

use bytes::Bytes;

/// Below this, deflate reliably costs more bytes than it saves: a short message
/// still pays the block header, and with no-context-takeover there is no
/// dictionary to amortize it against.
const MIN_COMPRESS_BYTES: usize = 128;

/// An encoded message, ready to become a frame for either kind of connection.
///
/// Holds the plain frame *once* and hands out the raw payload as a slice of it,
/// so preparing a broadcast is a single allocation no matter how many
/// connections receive it, and the compressed variant is built at most once per
/// shard rather than once per recipient.
#[derive(Debug, Clone)]
pub struct Outgoing {
    /// The complete frame — header included — with RSV1 clear.
    plain: Bytes,
    /// Where the payload starts inside `plain`.
    header_len: usize,
}

impl Outgoing {
    /// Frame an encoded message.
    pub fn text(payload: &[u8]) -> Self {
        let plain = frame::build(frame::OpCode::Text, false, payload);
        Self {
            header_len: plain.len() - payload.len(),
            plain,
        }
    }

    /// The frame to send to a connection that did not negotiate deflate.
    pub fn plain(&self) -> Bytes {
        self.plain.clone()
    }

    /// The uncompressed payload, without the frame header.
    pub fn payload(&self) -> Bytes {
        self.plain.slice(self.header_len..)
    }

    pub fn len(&self) -> usize {
        self.plain.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }

    /// Build the RSV1 variant.
    ///
    /// Falls back to the plain frame when compression would not pay — a short
    /// message, or one that deflate happens to expand. RSV1 is per-message, so
    /// mixing the two on one connection is legal and costs the peer nothing.
    pub fn deflated(&self, deflater: &mut deflate::Deflater) -> Bytes {
        let payload = self.payload();
        if payload.len() < MIN_COMPRESS_BYTES {
            return self.plain();
        }
        let compressed = deflater.compress(&payload);
        if compressed.len() >= payload.len() {
            return self.plain();
        }
        frame::build(frame::OpCode::Text, true, &compressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_a_slice_of_the_frame_not_a_copy() {
        let text = b"a message long enough to have a two-byte header only";
        let out = Outgoing::text(text);
        assert_eq!(&out.payload()[..], &text[..]);
        assert_eq!(out.len(), text.len() + 2);
        // Same allocation: a broadcast must not copy its payload per shard.
        assert_eq!(
            out.plain().as_ptr() as usize + out.header_len,
            out.payload().as_ptr() as usize
        );
    }

    #[test]
    fn a_compressible_message_gets_rsv1() {
        let payload = "{\"cmd\":\"PrintJSON\"}".repeat(40);
        let out = Outgoing::text(payload.as_bytes());
        let mut deflater = deflate::Deflater::new(6, deflate::WINDOW_BITS);
        let frame = out.deflated(&mut deflater);
        assert_eq!(frame[0] & 0x40, 0x40, "RSV1 should be set");
        assert!(frame.len() < out.len(), "and it should be smaller");
    }

    #[test]
    fn short_and_incompressible_messages_stay_plain() {
        let mut deflater = deflate::Deflater::new(6, deflate::WINDOW_BITS);

        // Too short to be worth a deflate block header.
        let short = Outgoing::text(b"[]");
        assert_eq!(short.deflated(&mut deflater), short.plain());

        // Long enough, but high-entropy enough that deflate cannot shrink it.
        // Sending an RSV1 frame larger than the original is legal and simply
        // worse, so the plain one wins.
        let mut state = 0x2545F4914F6CDD1Du64;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                // xorshift64: cheap, and genuinely incompressible unlike a
                // multiply-shift sequence, which deflate finds the pattern in.
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let out = Outgoing::text(&noise);
        assert_eq!(
            out.deflated(&mut deflater),
            out.plain(),
            "compression that expands a message should be skipped"
        );
    }
}
