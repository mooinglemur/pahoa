//! The opening handshake, and permessage-deflate negotiation.
//!
//! RFC 6455 §4.2 for the upgrade, RFC 7692 §7 for the extension. The
//! interesting half is the extension: what pahoa asks for here is what makes a
//! broadcast compressible once instead of once per connection.

use sha1::{Digest, Sha1};

/// RFC 6455 §1.3. Concatenated with the client's key before hashing; the value
/// is arbitrary and fixed, and exists only to prove both ends speak WebSocket.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandshakeError {
    #[error("request headers exceeded {0} bytes")]
    HeadersTooLarge(usize),
    #[error("malformed HTTP request: {0}")]
    Malformed(String),
    #[error("not a WebSocket upgrade")]
    NotAnUpgrade,
    #[error("unsupported WebSocket version {0:?}, this server speaks 13")]
    BadVersion(String),
    #[error("missing Sec-WebSocket-Key")]
    MissingKey,
}

/// What the client asked for, once validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub accept_key: String,
    /// The client's `permessage-deflate` offer, if it made one we can accept.
    pub deflate: Option<Offer>,
    /// Request target, kept for a future scoped-feed path.
    pub path: String,
}

/// A `permessage-deflate` offer, reduced to the parameters that matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Offer {
    /// The client can accept a smaller window for *our* stream.
    pub server_max_window_bits: Option<u8>,
    /// The client offered to accept a limit on its own window. Present without
    /// a value means "I understand the parameter"; RFC 7692 §7.1.2.2 only lets
    /// the server name `client_max_window_bits` when the client offered it.
    pub client_max_window_bits: Option<Option<u8>>,
    pub client_no_context_takeover: bool,
    pub server_no_context_takeover: bool,
}

/// What the server decided, ready to render into a response header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub server_max_window_bits: u8,
    pub client_max_window_bits: Option<u8>,
    pub client_no_context_takeover: bool,
}

impl Accepted {
    /// The `Sec-WebSocket-Extensions` value to send back.
    ///
    /// `server_no_context_takeover` is unconditional and is the whole point:
    /// it makes every message compress independently, so identical payloads
    /// produce identical bytes and one broadcast is compressed once rather than
    /// once per connection.
    pub fn header_value(&self) -> String {
        let mut out = String::from("permessage-deflate; server_no_context_takeover");
        out.push_str(&format!(
            "; server_max_window_bits={}",
            self.server_max_window_bits
        ));
        if let Some(bits) = self.client_max_window_bits {
            out.push_str(&format!("; client_max_window_bits={bits}"));
        }
        if self.client_no_context_takeover {
            out.push_str("; client_no_context_takeover");
        }
        out
    }
}

/// Settings the server applies when accepting an offer.
#[derive(Debug, Clone, Copy)]
pub struct DeflateConfig {
    pub enabled: bool,
    /// Window we ask to use for our own stream. Smaller costs ratio and saves
    /// nothing here — the compressor is stateless — but the client has to hold a
    /// matching window, and there are 6000 of those.
    pub server_max_window_bits: u8,
    /// Window we ask the client to use. Only sendable if the client offered the
    /// parameter, and it bounds *our* per-connection decompressor state.
    pub client_max_window_bits: u8,
    /// Ask the client to reset its compressor per message.
    ///
    /// Off by default, matching the reference server. It would bound our
    /// inbound state further, but at window bits 11 that state is ~10 KB per
    /// connection — 60 MB at 6000 — which is not worth costing every client
    /// their compression ratio.
    pub client_no_context_takeover: bool,
}

impl Default for DeflateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            server_max_window_bits: super::deflate::WINDOW_BITS,
            client_max_window_bits: super::deflate::WINDOW_BITS,
            client_no_context_takeover: false,
        }
    }
}

impl DeflateConfig {
    /// Decide what to answer an offer with, or `None` to decline.
    ///
    /// Declining is always safe: a client that offered compression and is
    /// answered without the extension simply does not use it. Archipelago's own
    /// client does exactly this today, which is what let M4 ship uncompressed.
    pub fn accept(&self, offer: &Offer) -> Option<Accepted> {
        if !self.enabled {
            return None;
        }
        // The client may cap what our stream is allowed to use. Never exceed
        // what it said it can handle.
        let server_bits = match offer.server_max_window_bits {
            Some(theirs) => self.server_max_window_bits.min(theirs),
            None => self.server_max_window_bits,
        };
        // Only nameable if they raised it first (RFC 7692 §7.1.2.2).
        let client_bits = offer.client_max_window_bits.map(|theirs| match theirs {
            Some(limit) => self.client_max_window_bits.min(limit),
            None => self.client_max_window_bits,
        });
        Some(Accepted {
            server_max_window_bits: server_bits.clamp(9, 15),
            client_max_window_bits: client_bits.map(|b| b.clamp(9, 15)),
            // If the client asked us to reset, we must; otherwise our own
            // preference applies.
            client_no_context_takeover: self.client_no_context_takeover
                || offer.client_no_context_takeover,
        })
    }
}

/// Find the end of the request headers, if it has arrived.
pub fn headers_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

/// Validate an upgrade request.
pub fn parse(buf: &[u8], max_headers: usize) -> Result<Request, HandshakeError> {
    if buf.len() > max_headers {
        return Err(HandshakeError::HeadersTooLarge(max_headers));
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(buf) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(HandshakeError::Malformed("incomplete".into()));
        }
        Err(e) => return Err(HandshakeError::Malformed(e.to_string())),
    }

    if !request
        .method
        .is_some_and(|m| m.eq_ignore_ascii_case("GET"))
    {
        return Err(HandshakeError::NotAnUpgrade);
    }
    let path = request.path.unwrap_or("/").to_string();

    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .and_then(|h| std::str::from_utf8(h.value).ok())
            .map(str::trim)
    };

    // `Connection` is a comma-separated list and may carry other tokens, so a
    // whole-value comparison would reject real clients.
    let upgrading = header("Connection").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    });
    let websocket = header("Upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if !upgrading || !websocket {
        return Err(HandshakeError::NotAnUpgrade);
    }

    match header("Sec-WebSocket-Version") {
        Some("13") => {}
        other => return Err(HandshakeError::BadVersion(other.unwrap_or("").to_string())),
    }

    let key = header("Sec-WebSocket-Key").ok_or(HandshakeError::MissingKey)?;
    let accept_key = accept(key);

    // Several offers may be listed; take the first `permessage-deflate` we can
    // parse, as RFC 7692 §5.1 directs.
    let deflate = header("Sec-WebSocket-Extensions").and_then(parse_extensions);

    Ok(Request {
        accept_key,
        deflate,
        path,
    })
}

/// `base64(sha1(key + GUID))` (RFC 6455 §4.2.2).
pub fn accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    base64(&hasher.finalize())
}

/// Standard base64. Hand-rolled because the only thing this server ever encodes
/// is a 20-byte digest, and that is not worth a dependency.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let indexes = [n >> 18 & 63, n >> 12 & 63, n >> 6 & 63, n & 63];
        for (i, index) in indexes.iter().enumerate() {
            // One input byte yields two characters, two yield three; the rest
            // is padding.
            if i <= chunk.len() {
                out.push(ALPHABET[*index as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Pull the first usable `permessage-deflate` offer out of the header value.
fn parse_extensions(value: &str) -> Option<Offer> {
    for extension in value.split(',') {
        let mut parts = extension.split(';').map(str::trim);
        let name = parts.next()?;
        if !name.eq_ignore_ascii_case("permessage-deflate") {
            continue;
        }
        let mut offer = Offer::default();
        let mut usable = true;
        for param in parts {
            let (key, val) = match param.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim().trim_matches('"'))),
                None => (param, None),
            };
            match key.to_ascii_lowercase().as_str() {
                "server_no_context_takeover" => offer.server_no_context_takeover = true,
                "client_no_context_takeover" => offer.client_no_context_takeover = true,
                "server_max_window_bits" => match val.map(str::parse::<u8>) {
                    Some(Ok(bits)) if (9..=15).contains(&bits) => {
                        offer.server_max_window_bits = Some(bits);
                    }
                    // Bare `server_max_window_bits` is not a legal offer, and a
                    // value we cannot honor makes the whole offer unusable
                    // rather than something to reinterpret.
                    _ => usable = false,
                },
                "client_max_window_bits" => match val.map(str::parse::<u8>) {
                    None => offer.client_max_window_bits = Some(None),
                    Some(Ok(bits)) if (9..=15).contains(&bits) => {
                        offer.client_max_window_bits = Some(Some(bits));
                    }
                    Some(_) => usable = false,
                },
                // An unknown parameter means this offer is not one we
                // understand; skip to the next rather than guess.
                _ => usable = false,
            }
        }
        if usable {
            return Some(offer);
        }
    }
    None
}

/// Render the 101 response.
pub fn response(accept_key: &str, deflate: Option<Accepted>) -> Vec<u8> {
    let mut out = String::with_capacity(256);
    out.push_str("HTTP/1.1 101 Switching Protocols\r\n");
    out.push_str("Upgrade: websocket\r\n");
    out.push_str("Connection: Upgrade\r\n");
    out.push_str(&format!("Sec-WebSocket-Accept: {accept_key}\r\n"));
    if let Some(accepted) = deflate {
        out.push_str(&format!(
            "Sec-WebSocket-Extensions: {}\r\n",
            accepted.header_value()
        ));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// A plain HTTP error, for requests that are not WebSocket upgrades at all.
pub fn error_response(status: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with(extra: &str) -> Vec<u8> {
        format!(
            "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             {extra}\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn the_accept_key_matches_the_rfc_example() {
        // RFC 6455 §1.3 works this exact pair, which makes it a known-answer
        // test for both the SHA-1 and the base64.
        assert_eq!(
            accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_pads_correctly_at_every_remainder() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_plain_upgrade_is_accepted() {
        let request = parse(&request_with(""), 8192).expect("valid upgrade");
        assert_eq!(request.accept_key, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(request.deflate, None);
        assert_eq!(request.path, "/");
    }

    #[test]
    fn non_upgrades_and_wrong_versions_are_refused() {
        let plain = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse(plain, 8192), Err(HandshakeError::NotAnUpgrade));

        let old = "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n"
            .to_string();
        assert_eq!(
            parse(old.as_bytes(), 8192),
            Err(HandshakeError::BadVersion("8".into()))
        );
    }

    #[test]
    fn a_connection_header_with_several_tokens_still_upgrades() {
        // Browsers and proxies send "keep-alive, Upgrade"; a whole-value
        // comparison would reject them.
        let request = "GET / HTTP/1.1\r\nUpgrade: WebSocket\r\nConnection: keep-alive, Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
            .to_string();
        assert!(parse(request.as_bytes(), 8192).is_ok());
    }

    #[test]
    fn the_offer_python_websockets_makes_is_understood() {
        // What Archipelago's client actually sends, since it uses `websockets`.
        let request = parse(
            &request_with(
                "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n",
            ),
            8192,
        )
        .unwrap();
        let offer = request.deflate.expect("a deflate offer");
        assert_eq!(offer.client_max_window_bits, Some(None));
        assert_eq!(offer.server_max_window_bits, None);

        let accepted = DeflateConfig::default().accept(&offer).unwrap();
        assert_eq!(accepted.server_max_window_bits, 11);
        assert_eq!(accepted.client_max_window_bits, Some(11));
        assert_eq!(
            accepted.header_value(),
            "permessage-deflate; server_no_context_takeover; \
             server_max_window_bits=11; client_max_window_bits=11"
        );
    }

    #[test]
    fn a_client_window_limit_is_never_exceeded() {
        // If the client says it can only manage 9 bits for our stream, asking
        // for 11 would produce data it cannot inflate.
        let request = parse(
            &request_with(
                "Sec-WebSocket-Extensions: permessage-deflate; server_max_window_bits=9; \
                 client_max_window_bits=10\r\n",
            ),
            8192,
        )
        .unwrap();
        let accepted = DeflateConfig::default()
            .accept(&request.deflate.unwrap())
            .unwrap();
        assert_eq!(accepted.server_max_window_bits, 9);
        assert_eq!(accepted.client_max_window_bits, Some(10));
    }

    #[test]
    fn client_max_window_bits_is_only_named_when_the_client_raised_it() {
        // RFC 7692 §7.1.2.2: naming it unprompted is a protocol error, and
        // Python's `websockets` fails the connection over it.
        let request = parse(
            &request_with("Sec-WebSocket-Extensions: permessage-deflate\r\n"),
            8192,
        )
        .unwrap();
        let accepted = DeflateConfig::default()
            .accept(&request.deflate.unwrap())
            .unwrap();
        assert_eq!(accepted.client_max_window_bits, None);
        assert!(!accepted.header_value().contains("client_max_window_bits"));
    }

    #[test]
    fn a_client_demanding_no_context_takeover_gets_it() {
        let request = parse(
            &request_with(
                "Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover\r\n",
            ),
            8192,
        )
        .unwrap();
        let accepted = DeflateConfig::default()
            .accept(&request.deflate.unwrap())
            .unwrap();
        assert!(accepted.client_no_context_takeover);
        assert!(
            accepted
                .header_value()
                .contains("client_no_context_takeover")
        );
    }

    #[test]
    fn an_offer_with_an_unknown_parameter_is_skipped_for_the_next_one() {
        // RFC 7692 §5.1 lists offers in preference order; one we cannot honor
        // means move on, not guess.
        let request = parse(
            &request_with(
                "Sec-WebSocket-Extensions: permessage-deflate; unknown_param=3, \
                 permessage-deflate; client_max_window_bits\r\n",
            ),
            8192,
        )
        .unwrap();
        let offer = request.deflate.expect("the second offer is usable");
        assert_eq!(offer.client_max_window_bits, Some(None));
    }

    #[test]
    fn deflate_can_be_declined_wholesale() {
        // The M4 finding: real clients tolerate a declined extension, so this
        // stays a supported configuration rather than a dead branch.
        let config = DeflateConfig {
            enabled: false,
            ..Default::default()
        };
        assert_eq!(config.accept(&Offer::default()), None);
        let bytes = response("key", None);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("Sec-WebSocket-Extensions"));
        assert!(text.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
    }

    #[test]
    fn headers_are_bounded() {
        let huge = vec![b'x'; 9000];
        assert_eq!(
            parse(&huge, 8192),
            Err(HandshakeError::HeadersTooLarge(8192))
        );
    }
}
