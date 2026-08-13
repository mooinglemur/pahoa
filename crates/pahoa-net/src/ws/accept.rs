//! Driving the handshake over a real socket.

use super::deflate::Inflater;
use super::handshake::{self, DeflateConfig, HandshakeError};
use bytes::BytesMut;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("handshake: {0}")]
    Handshake(#[from] HandshakeError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("client sent no handshake within {0:?}")]
    Timeout(Duration),
    #[error("connection closed during the handshake")]
    Closed,
    #[error("client tried TLS on a plaintext port; terminate TLS at a proxy")]
    Tls,
    #[error("not an HTTP request")]
    NotHttp,
}

#[derive(Debug, Clone, Copy)]
pub struct AcceptConfig {
    pub deflate: DeflateConfig,
    pub max_headers: usize,
    pub timeout: Duration,
    /// Cap on an inflated inbound message. A 2 KiB window still expands far
    /// enough to matter on a public endpoint.
    pub max_message: usize,
}

/// The result of a successful upgrade.
#[derive(Debug)]
pub struct Upgraded {
    /// `None` when the extension was declined or never offered.
    pub inflater: Option<Inflater>,
    /// The window size **we** agreed to compress with, when deflate was
    /// negotiated.
    ///
    /// A size rather than a flag, because a client may cap our window below
    /// what we would otherwise use and then inflate with that smaller window.
    /// Compressing with a larger one produces back-references it cannot
    /// resolve, which fails only once a payload is big enough to reach past the
    /// client's window — so the bug hides completely at small message sizes.
    pub deflate: Option<u8>,
    /// Bytes already read past the end of the headers.
    ///
    /// A client is allowed to pipeline its first frames into the same segment
    /// as the request, and Archipelago's does — dropping these loses the
    /// `Connect`.
    pub leftover: BytesMut,
    pub path: String,
}

/// Read the request, answer it, and report what was negotiated.
pub async fn accept<S>(stream: &mut S, config: &AcceptConfig) -> Result<Upgraded, AcceptError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = BytesMut::with_capacity(1024);
    let deadline = tokio::time::Instant::now() + config.timeout;

    let request = loop {
        if handshake::headers_complete(&buf) {
            break handshake::parse(&buf, config.max_headers)?;
        }
        if buf.len() > config.max_headers {
            return Err(HandshakeError::HeadersTooLarge(config.max_headers).into());
        }
        let read = tokio::time::timeout_at(deadline, stream.read_buf(&mut buf))
            .await
            .map_err(|_| AcceptError::Timeout(config.timeout))??;
        if read == 0 {
            return Err(AcceptError::Closed);
        }
        sniff(&buf)?;
    };

    let accepted = request
        .deflate
        .as_ref()
        .and_then(|offer| config.deflate.accept(offer));

    stream
        .write_all(&handshake::response(&request.accept_key, accepted))
        .await?;
    stream.flush().await?;

    // Everything after the blank line belongs to the WebSocket stream.
    let end = find_header_end(&buf).expect("headers were complete");
    let leftover = buf.split_off(end);

    let inflater = accepted.map(|a| {
        Inflater::new(
            // Our decompressor has to match the window the *client's*
            // compressor uses. If we never capped it, it may use the full 15
            // bits, and guessing lower would fail to inflate their traffic.
            a.client_max_window_bits.unwrap_or(15),
            a.client_no_context_takeover,
            config.max_message,
        )
    });

    Ok(Upgraded {
        deflate: accepted.map(|a| a.server_max_window_bits),
        inflater,
        leftover,
        path: request.path,
    })
}

/// Recognize a connection that cannot become an HTTP request, as soon as it is
/// recognizable, rather than waiting for a `\r\n\r\n` that will never arrive.
///
/// The case that motivated this: a client that tries `wss://` first and falls
/// back to `ws://` sends a TLS ClientHello and then waits for a ServerHello.
/// Those bytes contain no header terminator and the peer sends nothing further,
/// so the read loop sat here for the full 30-second handshake timeout and the
/// fallback looked like a hang. Neither side is at fault; nobody says anything.
fn sniff(buf: &[u8]) -> Result<(), AcceptError> {
    // A TLS record: handshake content type, then a major version of 3 (every
    // version from SSL 3.0 to TLS 1.3 puts a 3 here, since 1.3 keeps the
    // legacy record version for compatibility).
    if buf.first() == Some(&0x16) {
        return match buf.get(1) {
            // Not enough yet to tell TLS from a stray byte; read on.
            None => Ok(()),
            Some(0x03) => Err(AcceptError::Tls),
            Some(_) => Err(AcceptError::NotHttp),
        };
    }
    // Every HTTP method is uppercase ASCII, and an upgrade must be a `GET`.
    // Anything else is binary garbage or a protocol we do not speak.
    match buf.first() {
        Some(c) if !c.is_ascii_uppercase() => Err(AcceptError::NotHttp),
        _ => Ok(()),
    }
}

/// Answer a request that is not a WebSocket upgrade, so a browser or health
/// check gets an HTTP status rather than a silently dropped socket.
pub async fn reject<S>(stream: &mut S, error: &AcceptError)
where
    S: tokio::io::AsyncWrite + Unpin,
{
    // A TLS peer cannot read an HTTP response — it is waiting for a ServerHello
    // and would report a protocol error on anything else. A fatal alert is the
    // one thing it does understand, and it turns "connection reset" into a
    // clean handshake failure the client can fall back from immediately.
    if matches!(error, AcceptError::Tls) {
        const HANDSHAKE_FAILURE: &[u8] = &[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28];
        let _ = stream.write_all(HANDSHAKE_FAILURE).await;
        let _ = stream.flush().await;
        return;
    }

    let status = match error {
        AcceptError::Handshake(HandshakeError::BadVersion(_)) => "426 Upgrade Required",
        AcceptError::Handshake(HandshakeError::HeadersTooLarge(_)) => {
            "431 Request Header Fields Too Large"
        }
        AcceptError::Handshake(_) => "400 Bad Request",
        _ => return,
    };
    let _ = stream.write_all(&handshake::error_response(status)).await;
    let _ = stream.flush().await;
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn config() -> AcceptConfig {
        AcceptConfig {
            deflate: DeflateConfig::default(),
            max_headers: 8192,
            timeout: Duration::from_secs(5),
            max_message: 1 << 20,
        }
    }

    const REQUEST: &str = "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n";

    /// A duplex pair standing in for a socket.
    async fn exchange(request: &str) -> (Result<Upgraded, AcceptError>, String) {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let request = request.to_string();
        let writer = tokio::spawn(async move {
            client.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            // The server closes after answering, so read to end.
            let _ = client.read_to_end(&mut response).await;
            String::from_utf8_lossy(&response).into_owned()
        });
        let result = accept(&mut server, &config()).await;
        drop(server);
        let response = writer.await.unwrap();
        (result, response)
    }

    #[tokio::test]
    async fn a_plain_upgrade_is_answered_without_the_extension() {
        let (result, response) = exchange(&format!("{REQUEST}\r\n")).await;
        let upgraded = result.expect("upgrades");
        assert_eq!(upgraded.deflate, None);
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(!response.contains("Sec-WebSocket-Extensions"));
    }

    #[tokio::test]
    async fn deflate_is_negotiated_when_offered() {
        let (result, response) = exchange(&format!(
            "{REQUEST}Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n\r\n"
        ))
        .await;
        let upgraded = result.expect("upgrades");
        assert_eq!(upgraded.deflate, Some(11));
        assert!(upgraded.inflater.is_some());
        assert!(response.contains("server_no_context_takeover"));
        assert!(response.contains("server_max_window_bits=11"));
    }

    #[tokio::test]
    async fn a_client_capping_our_window_gets_that_size_reported_back() {
        // The Autobahn 13.3.9 bug: accepting a smaller window and then
        // compressing with the default produces back-references the client
        // cannot resolve. Whatever is agreed here is what must reach the
        // compressor, so the accept path has to report it rather than a flag.
        let (result, response) = exchange(&format!(
            "{REQUEST}Sec-WebSocket-Extensions: permessage-deflate; server_max_window_bits=9\r\n\r\n"
        ))
        .await;
        let upgraded = result.expect("upgrades");
        assert_eq!(upgraded.deflate, Some(9));
        assert!(response.contains("server_max_window_bits=9"));
    }

    #[tokio::test]
    async fn frames_pipelined_behind_the_handshake_are_kept() {
        // A client may put its first frames in the same segment as the request.
        // Archipelago's does, so losing these loses the `Connect`.
        let mut request = format!("{REQUEST}\r\n").into_bytes();
        request.extend_from_slice(&[0x81, 0x80, 0, 0, 0, 0]);
        let (mut client, mut server) = tokio::io::duplex(4096);
        client.write_all(&request).await.unwrap();

        let upgraded = accept(&mut server, &config()).await.expect("upgrades");
        assert_eq!(
            &upgraded.leftover[..],
            &[0x81, 0x80, 0, 0, 0, 0],
            "the pipelined frame must survive the handshake"
        );
    }

    #[tokio::test]
    async fn a_request_split_across_reads_still_completes() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let whole = format!("{REQUEST}\r\n");
        let (head, tail) = whole.split_at(40);
        let head = head.to_string();
        let tail = tail.to_string();
        tokio::spawn(async move {
            client.write_all(head.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            client.write_all(tail.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        assert!(accept(&mut server, &config()).await.is_ok());
    }

    #[tokio::test]
    async fn a_client_that_says_nothing_times_out() {
        let (_client, mut server) = tokio::io::duplex(4096);
        let config = AcceptConfig {
            timeout: Duration::from_millis(50),
            ..config()
        };
        assert!(matches!(
            accept(&mut server, &config).await,
            Err(AcceptError::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn a_non_upgrade_gets_an_http_error_rather_than_silence() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let error = accept(&mut server, &config()).await.unwrap_err();
        reject(&mut server, &error).await;
        drop(server);

        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "got {response:?}"
        );
    }

    /// A client that tries `wss://` before `ws://` used to hang here for the
    /// full handshake timeout: a ClientHello contains no `\r\n\r\n` and the
    /// peer then waits for a ServerHello, so neither side says anything.
    #[tokio::test]
    async fn a_tls_client_hello_fails_at_once_rather_than_at_the_timeout() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        // Opening bytes of a real ClientHello: handshake record, legacy
        // version, record length, then the handshake header.
        client
            .write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xfc])
            .await
            .unwrap();

        let config = AcceptConfig {
            timeout: Duration::from_secs(30),
            ..config()
        };
        let started = std::time::Instant::now();
        let error = accept(&mut server, &config).await.unwrap_err();

        assert!(matches!(error, AcceptError::Tls), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}, so it waited on the timeout",
            started.elapsed()
        );

        // And the peer gets a fatal alert, which its TLS stack reports as a
        // clean handshake failure instead of a connection reset.
        reject(&mut server, &error).await;
        drop(server);
        let mut alert = Vec::new();
        client.read_to_end(&mut alert).await.unwrap();
        assert_eq!(alert, [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]);
    }

    #[tokio::test]
    async fn binary_garbage_is_refused_rather_than_waited_on() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client.write_all(&[0x00, 0x01, 0x02, 0x03]).await.unwrap();
        let error = accept(&mut server, &config()).await.unwrap_err();
        assert!(matches!(error, AcceptError::NotHttp), "{error:?}");
    }

    #[tokio::test]
    async fn endless_headers_are_cut_off() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let junk = format!("X-Pad: {}\r\n", "a".repeat(1000));
            loop {
                if client.write_all(junk.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let config = AcceptConfig {
            max_headers: 4096,
            ..config()
        };
        assert!(matches!(
            accept(&mut server, &config).await,
            Err(AcceptError::Handshake(HandshakeError::HeadersTooLarge(_)))
        ));
    }
}
