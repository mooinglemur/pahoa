//! The client half of the WebSocket layer.
//!
//! Exists for the load driver, which has to negotiate permessage-deflate to
//! measure anything meaningful about compression — and no WebSocket crate does
//! that, which is why this layer is here in the first place. It is deliberately
//! minimal: no fragmentation on send, no close-handshake bookkeeping beyond
//! echoing, because the traffic it generates is Archipelago's, which does
//! neither.

use super::deflate::{Deflater, Inflater, WINDOW_BITS};
use super::frame::{self, OpCode, Role};
use super::message::{Event, Session};
use bytes::BytesMut;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A connection to a pahoa server, speaking the wire protocol properly.
pub struct Client {
    stream: TcpStream,
    session: Session,
    buf: BytesMut,
    deflater: Option<Deflater>,
    /// Masking keys need only be varied, not unpredictable — masking protects
    /// intermediaries from cache poisoning, not the payload from being read.
    counter: u32,
    pub deflate: bool,
}

impl Client {
    /// Connect and upgrade, offering deflate unless told otherwise.
    pub async fn connect(addr: std::net::SocketAddr, offer_deflate: bool) -> io::Result<Self> {
        let mut stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true).ok();

        let extensions = if offer_deflate {
            "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n"
        } else {
            ""
        };
        // A fixed key is fine: the server only echoes it back through SHA-1, and
        // nothing here depends on it being unguessable.
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             {extensions}\r\n"
        );
        stream.write_all(request.as_bytes()).await?;

        let mut buf = BytesMut::with_capacity(4096);
        let end = loop {
            if let Some(end) = find_header_end(&buf) {
                break end;
            }
            if stream.read_buf(&mut buf).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed during the handshake",
                ));
            }
        };
        let response = String::from_utf8_lossy(&buf[..end]).into_owned();
        if !response.starts_with("HTTP/1.1 101") {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("upgrade refused: {}", response.lines().next().unwrap_or("")),
            ));
        }
        // Anything past the blank line is already WebSocket traffic — pahoa
        // sends `RoomInfo` unprompted, so this is the common case rather than a
        // corner one.
        let leftover = buf.split_off(end);

        let accepted = parse_accepted(&response);
        let (deflater, inflater) = match accepted {
            Some((server_bits, client_bits, server_no_takeover)) => (
                // Ours compresses at the window *the server said it would read*.
                Some(Deflater::new(6, client_bits)),
                Some(Inflater::new(server_bits, server_no_takeover, 64 << 20)),
            ),
            None => (None, None),
        };

        Ok(Self {
            stream,
            deflate: deflater.is_some(),
            session: Session::new(inflater, 64 << 20),
            buf: leftover,
            deflater,
            counter: 0,
        })
    }

    /// Send one text message, compressing it when deflate was negotiated.
    pub async fn send(&mut self, text: &str) -> io::Result<()> {
        self.counter = self.counter.wrapping_add(0x9E37_79B9);
        let key = self.counter.to_be_bytes();
        let frame = match self.deflater.as_mut() {
            Some(deflater) if text.len() >= 128 => {
                let compressed = deflater.compress(text.as_bytes());
                if compressed.len() < text.len() {
                    frame::build_masked(OpCode::Text, true, &compressed, key)
                } else {
                    frame::build_masked(OpCode::Text, false, text.as_bytes(), key)
                }
            }
            _ => frame::build_masked(OpCode::Text, false, text.as_bytes(), key),
        };
        self.stream.write_all(&frame).await
    }

    /// Next message, answering pings on the way so the server never sees this
    /// connection as unresponsive.
    pub async fn recv(&mut self) -> io::Result<Option<String>> {
        loop {
            match frame::decode_as(&mut self.buf, 64 << 20, Role::Client) {
                Ok(Some(f)) => match self.session.handle(f) {
                    Ok(Some(Event::Text(text))) => return Ok(Some(text)),
                    Ok(Some(Event::Ping(payload))) => {
                        self.counter = self.counter.wrapping_add(0x9E37_79B9);
                        let pong = frame::build_masked(
                            OpCode::Pong,
                            false,
                            &payload,
                            self.counter.to_be_bytes(),
                        );
                        self.stream.write_all(&pong).await?;
                    }
                    Ok(Some(Event::Close(_))) => return Ok(None),
                    Ok(Some(Event::Binary(_)) | Some(Event::Pong(_)) | None) => {}
                    Err(e) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                    }
                },
                Ok(None) => {
                    if self.stream.read_buf(&mut self.buf).await? == 0 {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            }
        }
    }

    /// Read until a packet with this `cmd` arrives, discarding the rest.
    pub async fn wait_for(&mut self, cmd: &str) -> io::Result<Option<serde_json::Value>> {
        while let Some(text) = self.recv().await? {
            let Ok(packets) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
                continue;
            };
            for packet in packets {
                if packet.get("cmd").and_then(|c| c.as_str()) == Some(cmd) {
                    return Ok(Some(packet));
                }
            }
        }
        Ok(None)
    }

    /// Consume whatever has already arrived without waiting for more.
    ///
    /// The load driver's connections exist mostly to *receive*, and a client
    /// that stops reading is indistinguishable from one that is too slow — the
    /// server would drop it, and the run would measure the wrong thing.
    pub async fn drain_ready(&mut self) -> io::Result<usize> {
        let mut messages = 0;
        loop {
            match frame::decode_as(&mut self.buf, 64 << 20, Role::Client) {
                Ok(Some(f)) => {
                    if matches!(self.session.handle(f), Ok(Some(Event::Text(_)))) {
                        messages += 1;
                    }
                }
                Ok(None) => {
                    let mut chunk = [0u8; 65536];
                    match self.stream.try_read(&mut chunk) {
                        Ok(0) => return Ok(messages),
                        Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(messages),
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            }
        }
    }
}

/// The read half, for a caller that wants to receive continuously while
/// something else sends.
///
/// The load driver needs this: polling for readable bytes on a timer makes the
/// *client* the bottleneck, and the server would then drop it for lagging —
/// turning a measurement of the server into a measurement of the harness.
pub struct Reader {
    read: tokio::net::tcp::OwnedReadHalf,
    session: Session,
    buf: BytesMut,
}

/// The write half.
pub struct Writer {
    write: tokio::net::tcp::OwnedWriteHalf,
    deflater: Option<Deflater>,
    counter: u32,
}

impl Client {
    /// Split into halves that can be driven from separate tasks.
    pub fn into_split(self) -> (Reader, Writer) {
        let (read, write) = self.stream.into_split();
        (
            Reader {
                read,
                session: self.session,
                buf: self.buf,
            },
            Writer {
                write,
                deflater: self.deflater,
                counter: self.counter,
            },
        )
    }
}

impl Reader {
    /// Next message, awaiting rather than polling. `None` on close.
    ///
    /// Pings are *not* answered here — the writer half owns the socket for
    /// writing — but Archipelago's server does not ping, so nothing depends on
    /// it in this configuration.
    pub async fn recv(&mut self) -> io::Result<Option<String>> {
        loop {
            match frame::decode_as(&mut self.buf, 64 << 20, Role::Client) {
                Ok(Some(f)) => match self.session.handle(f) {
                    Ok(Some(Event::Text(text))) => return Ok(Some(text)),
                    Ok(Some(Event::Close(_))) => return Ok(None),
                    Ok(_) => {}
                    Err(e) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                    }
                },
                Ok(None) => {
                    if self.read.read_buf(&mut self.buf).await? == 0 {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            }
        }
    }
}

impl Reader {
    /// Consume frames without inflating or parsing them, counting messages.
    ///
    /// For load generation, where the question is what the *server* costs. A
    /// client that fully inflates every broadcast is far more expensive than the
    /// server that compressed it once — at 6000 connections the harness would
    /// become the bottleneck and the run would measure the harness. The server
    /// still does all of its own work: the extension is negotiated, the payload
    /// is compressed, the bytes are written.
    pub async fn discard(&mut self) -> io::Result<u64> {
        let mut messages = 0;
        loop {
            match frame::decode_as(&mut self.buf, 64 << 20, Role::Client) {
                Ok(Some(f)) => {
                    if f.fin && !f.opcode.is_control() {
                        messages += 1;
                    }
                    if f.opcode == OpCode::Close {
                        return Ok(messages);
                    }
                }
                Ok(None) => {
                    if self.read.read_buf(&mut self.buf).await? == 0 {
                        return Ok(messages);
                    }
                    // Keep the buffer from growing without bound on a firehose.
                    if self.buf.capacity() > (8 << 20) && self.buf.is_empty() {
                        self.buf = BytesMut::with_capacity(64 << 10);
                    }
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            }
        }
    }
}

impl Writer {
    pub async fn send(&mut self, text: &str) -> io::Result<()> {
        self.counter = self.counter.wrapping_add(0x9E37_79B9);
        let key = self.counter.to_be_bytes();
        let frame = match self.deflater.as_mut() {
            Some(deflater) if text.len() >= 128 => {
                let compressed = deflater.compress(text.as_bytes());
                if compressed.len() < text.len() {
                    frame::build_masked(OpCode::Text, true, &compressed, key)
                } else {
                    frame::build_masked(OpCode::Text, false, text.as_bytes(), key)
                }
            }
            _ => frame::build_masked(OpCode::Text, false, text.as_bytes(), key),
        };
        self.write.write_all(&frame).await
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Pull the negotiated parameters out of the response.
///
/// Returns `(server_max_window_bits, client_max_window_bits,
/// server_no_context_takeover)`.
fn parse_accepted(response: &str) -> Option<(u8, u8, bool)> {
    let line = response.lines().find(|l| {
        l.to_ascii_lowercase()
            .starts_with("sec-websocket-extensions:")
    })?;
    let value = line.split_once(':')?.1;
    if !value.to_ascii_lowercase().contains("permessage-deflate") {
        return None;
    }
    let bits = |name: &str| -> u8 {
        value
            .split(';')
            .map(str::trim)
            .find_map(|p| p.strip_prefix(name)?.strip_prefix('=')?.trim().parse().ok())
            // Absent means the peer did not constrain it, which per RFC 7692
            // means the maximum.
            .unwrap_or(15)
    };
    Some((
        bits("server_max_window_bits"),
        bits("client_max_window_bits").min(WINDOW_BITS.max(15)),
        value.contains("server_no_context_takeover"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_negotiated_parameters_are_read_back_out_of_the_response() {
        let response = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover; \
             server_max_window_bits=11; client_max_window_bits=11\r\n\r\n";
        assert_eq!(parse_accepted(response), Some((11, 11, true)));
    }

    #[test]
    fn a_response_without_the_extension_declines() {
        let response = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
        assert_eq!(parse_accepted(response), None);
    }

    #[test]
    fn absent_window_parameters_mean_the_maximum() {
        // RFC 7692: an omitted `*_max_window_bits` is not "the default we like",
        // it is "unconstrained" — guessing lower would fail to inflate.
        let response = "HTTP/1.1 101 Switching Protocols\r\n\
             Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n";
        assert_eq!(parse_accepted(response), Some((15, 15, false)));
    }
}
