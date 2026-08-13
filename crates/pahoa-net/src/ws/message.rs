//! Frames to messages: fragmentation, compression, and the control-frame rules.
//!
//! Pure — frames in, events out, no I/O — so every rule below is testable
//! without a socket, and so the reader task can own the decision while the
//! writer task owns the socket.
//!
//! The ordering here is the part that is easy to get wrong. RFC 7692 compresses
//! a **message**, not a frame, so a fragmented compressed message must be
//! reassembled *first*, then inflated, and only then validated as UTF-8. Doing
//! the UTF-8 check per fragment — which is what a library layered under deflate
//! naturally does — rejects every compressed text message, because compressed
//! bytes are not UTF-8.

use super::deflate::{DeflateError, Inflater};
use super::frame::{Frame, OpCode};
use bytes::{Bytes, BytesMut};

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("continuation frame with no message in progress")]
    OrphanContinuation,
    #[error("new data frame while a fragmented message was in progress")]
    InterleavedMessage,
    #[error("RSV1 set on a continuation frame; compression applies to a message")]
    Rsv1OnContinuation,
    #[error("compressed frame received but permessage-deflate was not negotiated")]
    UnexpectedCompression,
    #[error("message of {len} bytes exceeds the {limit}-byte limit")]
    TooLarge { len: usize, limit: usize },
    #[error("text message was not valid UTF-8")]
    NotUtf8,
    #[error("close frame carried a 1-byte payload")]
    ShortClose,
    #[error("close code {0} may not appear on the wire")]
    BadCloseCode(u16),
    #[error(transparent)]
    Deflate(#[from] DeflateError),
}

/// Something the connection has to act on.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Text(String),
    /// Archipelago is a text protocol, so the room never sees these — but the
    /// layer still has to reassemble and account for them correctly.
    Binary(Bytes),
    /// Answer with a pong carrying the same payload.
    Ping(Bytes),
    Pong(Bytes),
    /// The peer is closing; echo the code and hang up.
    Close(Option<(u16, String)>),
}

struct Partial {
    opcode: OpCode,
    compressed: bool,
    buf: BytesMut,
}

pub struct Session {
    inflater: Option<Inflater>,
    partial: Option<Partial>,
    max_message: usize,
}

impl Session {
    pub fn new(inflater: Option<Inflater>, max_message: usize) -> Self {
        Self {
            inflater,
            partial: None,
            max_message,
        }
    }

    /// Feed one frame. `None` means the message is still incomplete.
    pub fn handle(&mut self, frame: Frame) -> Result<Option<Event>, ProtocolError> {
        // Control frames are never fragmented and may arrive *between* the
        // fragments of a data message, so they are handled before any
        // continuation bookkeeping (RFC 6455 §5.4).
        if frame.opcode.is_control() {
            return self.control(frame).map(Some);
        }

        match frame.opcode {
            OpCode::Continuation => {
                if frame.rsv1 {
                    // RSV1 marks a *message*; repeating it on a continuation is
                    // a protocol error, not a redundant hint.
                    return Err(ProtocolError::Rsv1OnContinuation);
                }
                let Some(partial) = self.partial.as_mut() else {
                    return Err(ProtocolError::OrphanContinuation);
                };
                if partial.buf.len() + frame.payload.len() > self.max_message {
                    return Err(ProtocolError::TooLarge {
                        len: partial.buf.len() + frame.payload.len(),
                        limit: self.max_message,
                    });
                }
                partial.buf.extend_from_slice(&frame.payload);
            }
            OpCode::Text | OpCode::Binary => {
                if self.partial.is_some() {
                    return Err(ProtocolError::InterleavedMessage);
                }
                if frame.rsv1 && self.inflater.is_none() {
                    return Err(ProtocolError::UnexpectedCompression);
                }
                if frame.payload.len() > self.max_message {
                    return Err(ProtocolError::TooLarge {
                        len: frame.payload.len(),
                        limit: self.max_message,
                    });
                }
                self.partial = Some(Partial {
                    opcode: frame.opcode,
                    compressed: frame.rsv1,
                    buf: BytesMut::from(&frame.payload[..]),
                });
            }
            OpCode::Close | OpCode::Ping | OpCode::Pong => unreachable!("handled above"),
        }

        if !frame.fin {
            return Ok(None);
        }
        let partial = self.partial.take().expect("a fragment was just recorded");
        self.finish(partial).map(Some)
    }

    /// Reassembled — now inflate, then validate.
    fn finish(&mut self, partial: Partial) -> Result<Event, ProtocolError> {
        let payload = if partial.compressed {
            let inflater = self
                .inflater
                .as_mut()
                .ok_or(ProtocolError::UnexpectedCompression)?;
            Bytes::from(inflater.decompress(&partial.buf)?)
        } else {
            partial.buf.freeze()
        };

        if payload.len() > self.max_message {
            return Err(ProtocolError::TooLarge {
                len: payload.len(),
                limit: self.max_message,
            });
        }

        match partial.opcode {
            OpCode::Text => {
                // After inflate, never before.
                let text = std::str::from_utf8(&payload)
                    .map_err(|_| ProtocolError::NotUtf8)?
                    .to_owned();
                Ok(Event::Text(text))
            }
            _ => Ok(Event::Binary(payload)),
        }
    }

    fn control(&mut self, frame: Frame) -> Result<Event, ProtocolError> {
        match frame.opcode {
            OpCode::Ping => Ok(Event::Ping(frame.payload)),
            OpCode::Pong => Ok(Event::Pong(frame.payload)),
            OpCode::Close => {
                let payload = &frame.payload;
                match payload.len() {
                    0 => Ok(Event::Close(None)),
                    // A lone byte cannot be a status code.
                    1 => Err(ProtocolError::ShortClose),
                    _ => {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        if !close_code_allowed(code) {
                            return Err(ProtocolError::BadCloseCode(code));
                        }
                        let reason = std::str::from_utf8(&payload[2..])
                            .map_err(|_| ProtocolError::NotUtf8)?
                            .to_owned();
                        Ok(Event::Close(Some((code, reason))))
                    }
                }
            }
            _ => unreachable!("not a control opcode"),
        }
    }
}

/// Which close codes a peer may actually send (RFC 6455 §7.4.1).
///
/// 1004 is undefined, and 1005/1006/1015 are reserved for *local* reporting —
/// a peer that puts them on the wire is misbehaving.
fn close_code_allowed(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999)
}

#[cfg(test)]
mod tests {
    use super::super::deflate::{Deflater, WINDOW_BITS};
    use super::*;

    fn frame(opcode: OpCode, fin: bool, rsv1: bool, payload: &[u8]) -> Frame {
        Frame {
            fin,
            rsv1,
            opcode,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    fn plain() -> Session {
        Session::new(None, 1 << 20)
    }

    fn compressed() -> Session {
        Session::new(Some(Inflater::new(WINDOW_BITS, true, 1 << 20)), 1 << 20)
    }

    #[test]
    fn a_whole_text_frame_is_a_message() {
        let mut s = plain();
        let event = s.handle(frame(OpCode::Text, true, false, b"hi")).unwrap();
        assert_eq!(event, Some(Event::Text("hi".into())));
    }

    #[test]
    fn fragments_are_reassembled_in_order() {
        let mut s = plain();
        assert_eq!(
            s.handle(frame(OpCode::Text, false, false, b"one "))
                .unwrap(),
            None
        );
        assert_eq!(
            s.handle(frame(OpCode::Continuation, false, false, b"two "))
                .unwrap(),
            None
        );
        assert_eq!(
            s.handle(frame(OpCode::Continuation, true, false, b"three"))
                .unwrap(),
            Some(Event::Text("one two three".into()))
        );
    }

    #[test]
    fn control_frames_may_arrive_between_fragments() {
        // RFC 6455 §5.4 explicitly allows this, and a naive reassembler that
        // treats every frame as part of the message corrupts the payload.
        let mut s = plain();
        s.handle(frame(OpCode::Text, false, false, b"before "))
            .unwrap();
        assert_eq!(
            s.handle(frame(OpCode::Ping, true, false, b"ka")).unwrap(),
            Some(Event::Ping(Bytes::from_static(b"ka")))
        );
        assert_eq!(
            s.handle(frame(OpCode::Continuation, true, false, b"after"))
                .unwrap(),
            Some(Event::Text("before after".into()))
        );
    }

    #[test]
    fn a_compressed_message_inflates_before_it_is_validated_as_utf8() {
        // The ordering that a library layered under deflate gets wrong:
        // compressed bytes are not UTF-8, so validating per frame rejects every
        // compressed text message.
        let text = "hello, compressed world — with non-ASCII";
        let payload = Deflater::new(6, WINDOW_BITS).compress(text.as_bytes());
        assert!(
            std::str::from_utf8(&payload).is_err(),
            "want non-UTF-8 bytes"
        );

        let mut s = compressed();
        assert_eq!(
            s.handle(frame(OpCode::Text, true, true, &payload)).unwrap(),
            Some(Event::Text(text.into()))
        );
    }

    #[test]
    fn a_fragmented_compressed_message_is_reassembled_before_inflating() {
        // Compression applies to the message, so neither half inflates alone.
        let text = "a message long enough to be worth splitting across two frames";
        let payload = Deflater::new(6, WINDOW_BITS).compress(text.as_bytes());
        let (head, tail) = payload.split_at(payload.len() / 2);

        let mut s = compressed();
        // RSV1 on the first frame only.
        assert_eq!(
            s.handle(frame(OpCode::Text, false, true, head)).unwrap(),
            None
        );
        assert_eq!(
            s.handle(frame(OpCode::Continuation, true, false, tail))
                .unwrap(),
            Some(Event::Text(text.into()))
        );
    }

    #[test]
    fn fragmentation_rules_are_enforced() {
        let mut s = plain();
        assert!(matches!(
            s.handle(frame(OpCode::Continuation, true, false, b"x")),
            Err(ProtocolError::OrphanContinuation)
        ));

        let mut s = plain();
        s.handle(frame(OpCode::Text, false, false, b"start"))
            .unwrap();
        assert!(matches!(
            s.handle(frame(OpCode::Text, true, false, b"interrupt")),
            Err(ProtocolError::InterleavedMessage)
        ));

        let mut s = compressed();
        s.handle(frame(OpCode::Text, false, true, b"start"))
            .unwrap();
        assert!(matches!(
            s.handle(frame(OpCode::Continuation, true, true, b"x")),
            Err(ProtocolError::Rsv1OnContinuation)
        ));
    }

    #[test]
    fn compression_without_negotiation_is_refused() {
        // RSV1 has no meaning unless an extension defined it, so a client
        // setting it on a plain connection is a protocol error.
        let mut s = plain();
        assert!(matches!(
            s.handle(frame(OpCode::Text, true, true, b"\x00\x01")),
            Err(ProtocolError::UnexpectedCompression)
        ));
    }

    #[test]
    fn invalid_utf8_is_refused() {
        let mut s = plain();
        assert!(matches!(
            s.handle(frame(OpCode::Text, true, false, &[0xff, 0xfe])),
            Err(ProtocolError::NotUtf8)
        ));

        // Binary carries the same bytes without complaint.
        let mut s = plain();
        assert!(matches!(
            s.handle(frame(OpCode::Binary, true, false, &[0xff, 0xfe])),
            Ok(Some(Event::Binary(_)))
        ));
    }

    #[test]
    fn a_message_split_mid_character_still_validates_as_a_whole() {
        // "€" is three bytes; neither fragment is valid UTF-8 alone. This is
        // why validation belongs after reassembly.
        let text = "€";
        let bytes = text.as_bytes();
        let mut s = plain();
        assert_eq!(
            s.handle(frame(OpCode::Text, false, false, &bytes[..1]))
                .unwrap(),
            None
        );
        assert_eq!(
            s.handle(frame(OpCode::Continuation, true, false, &bytes[1..]))
                .unwrap(),
            Some(Event::Text(text.into()))
        );
    }

    #[test]
    fn oversized_messages_are_refused_across_fragments_too() {
        let mut s = Session::new(None, 16);
        assert!(matches!(
            s.handle(frame(OpCode::Text, true, false, &[b'x'; 17])),
            Err(ProtocolError::TooLarge { .. })
        ));

        // And the limit is on the whole message, not each fragment.
        let mut s = Session::new(None, 16);
        s.handle(frame(OpCode::Text, false, false, &[b'x'; 10]))
            .unwrap();
        assert!(matches!(
            s.handle(frame(OpCode::Continuation, true, false, &[b'x'; 10])),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_compressed_message_is_capped_on_its_inflated_size() {
        // The cap that matters: the compressed frame is small and legal, and
        // only inflating reveals the problem.
        let payload = Deflater::new(9, WINDOW_BITS).compress(&vec![0u8; 1 << 20]);
        assert!(payload.len() < 4096, "want a small bomb");

        let mut s = Session::new(Some(Inflater::new(WINDOW_BITS, true, 4096)), 1 << 20);
        assert!(matches!(
            s.handle(frame(OpCode::Text, true, true, &payload)),
            Err(ProtocolError::Deflate(DeflateError::TooLarge { .. }))
        ));
    }

    #[test]
    fn close_frames_are_validated() {
        let mut s = plain();
        assert_eq!(
            s.handle(frame(OpCode::Close, true, false, b"")).unwrap(),
            Some(Event::Close(None))
        );

        let mut s = plain();
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        assert_eq!(
            s.handle(frame(OpCode::Close, true, false, &payload))
                .unwrap(),
            Some(Event::Close(Some((1000, "bye".into()))))
        );

        // A single byte cannot be a status code.
        let mut s = plain();
        assert!(matches!(
            s.handle(frame(OpCode::Close, true, false, b"\x03")),
            Err(ProtocolError::ShortClose)
        ));
    }

    #[test]
    fn reserved_close_codes_are_refused() {
        // 1005 and 1006 are for local reporting and must never be sent; 1004
        // and 1015 are undefined.
        for code in [999u16, 1004, 1005, 1006, 1015, 1016, 2999, 5000] {
            let mut s = plain();
            assert!(
                matches!(
                    s.handle(frame(OpCode::Close, true, false, &code.to_be_bytes())),
                    Err(ProtocolError::BadCloseCode(_))
                ),
                "close code {code} should be refused"
            );
        }
        for code in [1000u16, 1001, 1003, 1007, 1011, 3000, 4999] {
            let mut s = plain();
            assert!(
                s.handle(frame(OpCode::Close, true, false, &code.to_be_bytes()))
                    .is_ok(),
                "close code {code} should be allowed"
            );
        }
    }
}
