//! permessage-deflate (RFC 7692).
//!
//! The wire format is raw DEFLATE with a quirk: a compressed message is the
//! deflate stream flushed with `Z_SYNC_FLUSH` and then **stripped of its
//! trailing `00 00 FF FF`** (§7.2.1). The receiver appends those four bytes
//! back before inflating. That empty-stored-block marker is what lets a
//! compressor emit a complete message without ending the stream, and losing
//! either half of the trick produces data that inflates to nothing.
//!
//! # Why `server_no_context_takeover` is the load-bearing option
//!
//! With context takeover the compressor keeps its window across messages, so
//! the same payload compresses to *different bytes* for every connection and a
//! broadcast has to be compressed once per recipient. At 6000 connections and
//! ~2,860 frames in a mass release that is 17 million compressions.
//!
//! Declaring `server_no_context_takeover` makes each message compress
//! independently, so **identical payloads produce identical bytes** and one
//! broadcast is compressed once and shared. It also bounds memory: retained
//! compressor state runs to tens of KB per connection, which at 6000
//! connections would be gigabytes of window alone.
//!
//! The cost is a slightly worse ratio — no cross-message dictionary. For a
//! repetitive `PrintJSON` firehose that is a small loss against a ~6000×
//! saving. The reference server leaves context takeover on
//! (`MultiServer.py:57-61`); this is a deliberate, spec-compliant divergence.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use std::sync::atomic::{AtomicU64, Ordering};

/// Total outbound messages compressed since start.
///
/// The plan names this as a number to watch, and it is the one that says
/// whether the whole design is working: it should track *broadcasts*, not
/// broadcasts times connections. If it ever approaches the latter,
/// `server_no_context_takeover` did not negotiate and every recipient is paying
/// for its own compression pass.
static COMPRESSIONS: AtomicU64 = AtomicU64::new(0);

/// How many messages have been compressed since the process started.
pub fn compressions() -> u64 {
    COMPRESSIONS.load(Ordering::Relaxed)
}

/// The `Z_SYNC_FLUSH` marker RFC 7692 strips from every compressed message.
const SYNC_TRAILER: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

/// Window size Archipelago negotiates, and the smallest DEFLATE allows.
///
/// 2 KiB of window per direction. The reference asks for this on both sides
/// (`MultiServer.py:57-61`), and at 6000 connections the inbound half is the
/// one that matters: it is state pahoa has to hold.
pub const WINDOW_BITS: u8 = 11;

#[derive(Debug, thiserror::Error)]
pub enum DeflateError {
    #[error("compressed message expanded past the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("malformed compressed message: {0}")]
    Corrupt(String),
}

/// Compresses whole messages, statelessly.
///
/// One of these serves every connection, because with no-context-takeover the
/// output depends only on the input.
pub struct Deflater {
    compress: Compress,
}

impl Deflater {
    /// `window_bits` must be what the handshake actually agreed for this
    /// server's stream, not the default: a peer that capped it will inflate
    /// with the smaller window and cannot resolve back-references past it.
    pub fn new(level: u32, window_bits: u8) -> Self {
        Self {
            compress: Compress::new_with_window_bits(Compression::new(level), false, window_bits),
        }
    }

    /// Compress one message, returning the payload for a frame with RSV1 set.
    pub fn compress(&mut self, input: &[u8]) -> Vec<u8> {
        COMPRESSIONS.fetch_add(1, Ordering::Relaxed);
        // Reset first, not after: `no_context_takeover` means each message must
        // start from an empty window, and resetting up front also recovers from
        // any half-finished state a previous error left behind.
        self.compress.reset();

        let mut out = Vec::with_capacity(input.len() / 3 + 32);
        let mut consumed = 0;
        loop {
            let before_in = self.compress.total_in();
            let before_out = self.compress.total_out();
            // `compress_vec` will not grow the Vec itself, so it needs headroom
            // to write into or it returns `BufError` forever.
            out.reserve(256.max(input.len() / 4));
            let status = self
                .compress
                .compress_vec(&input[consumed..], &mut out, FlushCompress::Sync)
                .expect("deflate of an in-memory buffer cannot fail");
            consumed += (self.compress.total_in() - before_in) as usize;
            let produced = self.compress.total_out() - before_out;

            // Done once everything is consumed and a flush cycle added nothing.
            if consumed == input.len() && (produced == 0 || out.ends_with(&SYNC_TRAILER)) {
                break;
            }
            if matches!(status, Status::StreamEnd) {
                break;
            }
        }

        // RFC 7692 §7.2.1: the trailing empty block is implied, not sent.
        if out.ends_with(&SYNC_TRAILER) {
            out.truncate(out.len() - SYNC_TRAILER.len());
        }
        out
    }
}

/// Decompresses whole messages for **one** connection.
///
/// Per-connection because the client chooses whether to keep its context; if it
/// does, this side has to keep the matching window. That is the asymmetry:
/// outbound compression is shared, inbound decompression cannot be.
#[derive(Debug)]
pub struct Inflater {
    decompress: Decompress,
    no_context_takeover: bool,
    /// Refuses a deflate bomb. A 2 KiB window can still expand ~1000×, and this
    /// is a public endpoint.
    limit: usize,
}

impl Inflater {
    pub fn new(window_bits: u8, no_context_takeover: bool, limit: usize) -> Self {
        Self {
            decompress: Decompress::new_with_window_bits(false, window_bits),
            no_context_takeover,
            limit,
        }
    }

    pub fn decompress(&mut self, input: &[u8]) -> Result<Vec<u8>, DeflateError> {
        if self.no_context_takeover {
            self.decompress.reset(false);
        }

        // The four bytes the sender was required to strip.
        let mut framed = Vec::with_capacity(input.len() + SYNC_TRAILER.len());
        framed.extend_from_slice(input);
        framed.extend_from_slice(&SYNC_TRAILER);

        let mut out = Vec::with_capacity(input.len() * 4);
        let mut consumed = 0;
        loop {
            let before_in = self.decompress.total_in();
            if out.len() >= self.limit {
                return Err(DeflateError::TooLarge { limit: self.limit });
            }
            // Grow geometrically, but never reserve past what the limit allows:
            // the allocation stays bounded however large the payload claims to
            // expand to. The `+ 1` leaves room to write one byte past the limit,
            // which is what makes exceeding it detectable rather than merely
            // suspected.
            //
            // Deliberately not `clamp`: it panics when the lower bound exceeds
            // the upper one, which happens as soon as fewer than 1024 bytes of
            // headroom remain — a payload inflating to just under the cap would
            // abort the process rather than be refused.
            let remaining = self.limit - out.len() + 1;
            let headroom = (out.len() * 2).max(1024).min(remaining);
            out.reserve(headroom);

            let status = self
                .decompress
                .decompress_vec(&framed[consumed..], &mut out, FlushDecompress::Sync)
                .map_err(|e| DeflateError::Corrupt(e.to_string()))?;
            consumed += (self.decompress.total_in() - before_in) as usize;

            if out.len() > self.limit {
                return Err(DeflateError::TooLarge { limit: self.limit });
            }
            match status {
                Status::StreamEnd => break,
                _ if consumed == framed.len() => break,
                // No input taken and no output made means it cannot progress;
                // without this a corrupt payload spins forever.
                Status::BufError if out.capacity() > out.len() => {
                    return Err(DeflateError::Corrupt("stalled".into()));
                }
                _ => {}
            }
        }

        if self.no_context_takeover {
            self.decompress.reset(false);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(payload: &[u8]) -> Vec<u8> {
        let mut d = Deflater::new(6, WINDOW_BITS);
        let mut i = Inflater::new(WINDOW_BITS, true, 1 << 20);
        let compressed = d.compress(payload);
        i.decompress(&compressed).expect("round trips")
    }

    #[test]
    fn messages_round_trip() {
        for payload in [
            &b""[..],
            b"x",
            b"hello world",
            // The shape that actually goes over this wire.
            br#"[{"cmd":"PrintJSON","data":[{"text":"1","type":"player_id"}]}]"#,
        ] {
            assert_eq!(round_trip(payload), payload, "{payload:?}");
        }
    }

    #[test]
    fn a_large_repetitive_payload_compresses_well_and_survives() {
        // A `PrintJSON` chunk is 140 near-identical packets, which is the case
        // the whole extension exists for.
        let packet = br#"{"cmd":"PrintJSON","type":"ItemSend","data":[{"text":"12","type":"player_id"},{"text":" sent ","type":"text"}]}"#;
        let mut payload = Vec::new();
        payload.push(b'[');
        for i in 0..140 {
            if i > 0 {
                payload.push(b',');
            }
            payload.extend_from_slice(packet);
        }
        payload.push(b']');

        let mut d = Deflater::new(6, WINDOW_BITS);
        let compressed = d.compress(&payload);
        assert_eq!(round_trip(&payload), payload);
        assert!(
            compressed.len() * 10 < payload.len(),
            "expected better than 10x on a repetitive chunk, got {} -> {}",
            payload.len(),
            compressed.len()
        );
    }

    #[test]
    fn the_same_input_always_produces_the_same_bytes() {
        // This is the property the whole broadcast design rests on: without it
        // a shared frame is not shareable, and every connection needs its own
        // compression pass.
        let payload = b"the same message, sent to everybody at once";
        let mut d = Deflater::new(6, WINDOW_BITS);
        let first = d.compress(payload);
        let second = d.compress(payload);
        let third = Deflater::new(6, WINDOW_BITS).compress(payload);
        assert_eq!(first, second, "no-context-takeover must be stateless");
        assert_eq!(first, third, "a fresh compressor must agree");
    }

    #[test]
    fn the_sync_trailer_is_stripped() {
        let mut d = Deflater::new(6, WINDOW_BITS);
        let compressed = d.compress(b"anything at all");
        assert!(
            !compressed.ends_with(&SYNC_TRAILER),
            "RFC 7692 requires the trailing empty block to be removed"
        );
    }

    #[test]
    fn a_deflate_bomb_is_refused() {
        // A megabyte of zeroes compresses to a little over a kilobyte; a real
        // bomb does far better. The cap has to bite on the *output*, since the
        // input tells you nothing.
        let payload = vec![0u8; 4 << 20];
        let mut d = Deflater::new(9, WINDOW_BITS);
        let compressed = d.compress(&payload);
        assert!(compressed.len() < 64 * 1024, "want a small bomb to send");

        let mut i = Inflater::new(WINDOW_BITS, true, 64 * 1024);
        assert!(
            matches!(
                i.decompress(&compressed),
                Err(DeflateError::TooLarge { .. })
            ),
            "a payload past the cap must be refused"
        );

        // And the same payload is fine when the cap allows it.
        let mut generous = Inflater::new(WINDOW_BITS, true, 8 << 20);
        assert_eq!(
            generous.decompress(&compressed).unwrap().len(),
            payload.len()
        );
    }

    #[test]
    fn the_window_size_changes_the_output_and_so_must_be_honored() {
        // Autobahn 13.3.9 found this: a client may cap our window below the
        // default and will then inflate with *that* window, so compressing with
        // a larger one emits back-references it cannot resolve. It only shows
        // up once a payload is long enough to reach past the client's window,
        // which is why every small test missed it.
        //
        // The mismatch is deliberately *not* asserted by decompressing here.
        // zlib-rs's raw inflate is lenient about an oversized window, so this
        // side cannot reproduce the peer's failure — real clients (Python
        // `websockets`, Autobahn) do reject it, and that is where the end-to-end
        // check lives. What this pins is the fact that made the bug possible:
        // the window is part of the input, so a compressor chosen without it is
        // simply wrong, and a memo keyed only on the payload would be too.
        let marker = "a distinctive run of bytes that deflate will back-reference. ";
        let mut state = 0x9E3779B97F4A7C15u64;
        let filler: String = (0..1200)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (b'a' + (state % 26) as u8) as char
            })
            .collect();
        // The repeat sits ~1.2 KB back: reachable by an 11-bit window (2 KiB),
        // out of reach for a 9-bit one (512 B).
        let payload = format!("{marker}{filler}{marker}").into_bytes();

        let wide = Deflater::new(6, 11).compress(&payload);
        let narrow = Deflater::new(6, 9).compress(&payload);
        assert_ne!(
            wide, narrow,
            "the window is part of the compressor's input; if these matched, \
             nothing here would depend on getting it right"
        );
        assert!(
            wide.len() < narrow.len(),
            "the larger window should find the distant match: {} vs {}",
            wide.len(),
            narrow.len()
        );

        // And each round-trips at the size it was compressed for.
        for (bits, compressed) in [(11u8, &wide), (9, &narrow)] {
            let mut reader = Inflater::new(bits, true, 1 << 20);
            assert_eq!(
                reader.decompress(compressed).unwrap(),
                payload,
                "bits {bits}"
            );
        }
    }

    #[test]
    fn every_legal_window_size_round_trips() {
        let payload = "a repetitive payload worth compressing. ".repeat(500);
        for bits in 9..=15u8 {
            let compressed = Deflater::new(6, bits).compress(payload.as_bytes());
            let mut inflater = Inflater::new(bits, true, 1 << 20);
            assert_eq!(
                inflater.decompress(&compressed).unwrap(),
                payload.as_bytes(),
                "window bits {bits}"
            );
        }
    }

    #[test]
    fn a_payload_landing_near_the_cap_is_refused_rather_than_crashing() {
        // A `clamp` whose lower bound exceeds its upper bound panics, and with
        // `panic = "abort"` in release that is a remote crash rather than a
        // refused message. The window is narrow — the last kilobyte before the
        // cap — so it needs sizes either side of the boundary to catch.
        for limit in [4096usize, 65536] {
            for size in [limit - 1, limit, limit + 1, limit + 1024] {
                let payload = vec![b'z'; size];
                let compressed = Deflater::new(6, WINDOW_BITS).compress(&payload);
                let mut inflater = Inflater::new(WINDOW_BITS, true, limit);
                match inflater.decompress(&compressed) {
                    Ok(out) => assert!(
                        out.len() <= limit,
                        "limit {limit}: returned {} bytes",
                        out.len()
                    ),
                    Err(DeflateError::TooLarge { .. }) => {
                        assert!(size > limit, "limit {limit}: refused a {size}-byte payload");
                    }
                    Err(e) => panic!("limit {limit}, size {size}: {e}"),
                }
            }
        }
    }

    #[test]
    fn garbage_is_an_error_rather_than_a_hang() {
        let mut i = Inflater::new(WINDOW_BITS, true, 1 << 20);
        assert!(i.decompress(&[0xff, 0x00, 0x13, 0x37]).is_err());
    }

    #[test]
    fn a_client_keeping_its_context_is_followed_across_messages() {
        // Without `client_no_context_takeover` the peer's compressor carries its
        // window forward, so ours has to as well — decompressing message two in
        // isolation would fail.
        let mut d = Compress::new_with_window_bits(Compression::new(6), false, WINDOW_BITS);
        let mut stateful = |input: &[u8]| {
            let mut out = Vec::with_capacity(input.len() + 256);
            d.compress_vec(input, &mut out, FlushCompress::Sync)
                .unwrap();
            if out.ends_with(&SYNC_TRAILER) {
                out.truncate(out.len() - SYNC_TRAILER.len());
            }
            out
        };
        let first = stateful(b"a repeated phrase worth remembering");
        let second = stateful(b"a repeated phrase worth remembering");
        assert!(second.len() < first.len(), "want the window to be in use");

        let mut i = Inflater::new(WINDOW_BITS, false, 1 << 20);
        assert_eq!(
            i.decompress(&first).unwrap(),
            b"a repeated phrase worth remembering"
        );
        assert_eq!(
            i.decompress(&second).unwrap(),
            b"a repeated phrase worth remembering"
        );
    }
}
