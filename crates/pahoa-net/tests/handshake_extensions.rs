//! How the server answers a client that offers `permessage-deflate`.
//!
//! Written at M4, when pahoa declined the extension and the open question was
//! whether real clients tolerate that. They do. M8 turned it on, so this now
//! covers both directions — the negotiation pahoa performs by default, and the
//! declining path, which stays a supported configuration rather than becoming a
//! dead branch.
//!
//! The offers exercised here are literally what Python's `websockets` sends,
//! since that is the library Archipelago's client uses.

use pahoa_multidata::MultiData;
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn load() -> Option<Arc<MultiData>> {
    let dir = std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .unwrap()
                .join("crates/pahoa-pickle/tests/fixtures")
        });
    let raw = std::fs::read(dir.join(FIXTURE)).ok()?;
    Some(Arc::new(MultiData::parse(&raw).expect("fixture parses")))
}

async fn start(data: Arc<MultiData>) -> Server {
    start_with(data, NetConfig::default()).await
}

async fn start_with(data: Arc<MultiData>, config: NetConfig) -> Server {
    let (names, _) = data.resolve_datapackage();
    let room = Room::new(data, Arc::new(names), RoomOptions::default(), 0.0);
    Server::start(room, NetConfig { port: 0, ..config })
        .await
        .unwrap()
}

/// Perform the HTTP upgrade by hand so the offered extensions are controllable.
async fn raw_handshake(addr: std::net::SocketAddr, extensions: Option<&str>) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    let mut request = format!(
        "GET / HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n"
    );
    if let Some(ext) = extensions {
        request.push_str(&format!("Sec-WebSocket-Extensions: {ext}\r\n"));
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("handshake should not hang")
        .expect("read response");
    String::from_utf8_lossy(&buf[..n]).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_upgrade_succeeds_without_extensions() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture not present");
        return;
    };
    let server = start(data).await;

    let response = raw_handshake(server.local_addr, None).await;
    assert!(response.starts_with("HTTP/1.1 101"), "{response}");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permessage_deflate_is_negotiated_with_no_context_takeover() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture not present");
        return;
    };
    let server = start(data).await;

    // Exactly what Python's `websockets` offers by default, which is what the
    // Archipelago client sends.
    let response = raw_handshake(
        server.local_addr,
        Some("permessage-deflate; client_max_window_bits"),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 101"),
        "upgrade must succeed:\n{response}"
    );
    let lower = response.to_ascii_lowercase();
    assert!(
        lower.contains("sec-websocket-extensions: permessage-deflate"),
        "the extension should be accepted:\n{response}"
    );
    // The whole reason M8 exists: without this, identical payloads compress to
    // different bytes per connection and a broadcast costs one compression per
    // recipient instead of one in total.
    assert!(
        lower.contains("server_no_context_takeover"),
        "server_no_context_takeover is what makes a broadcast shareable:\n{response}"
    );
    assert!(
        lower.contains("server_max_window_bits=11"),
        "window bits should match what the reference negotiates:\n{response}"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_max_window_bits_is_not_named_unless_the_client_offered_it() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture not present");
        return;
    };
    let server = start(data).await;

    // RFC 7692 §7.1.2.2. Naming the parameter unprompted is a protocol error,
    // and Python's `websockets` fails the connection over it — so this is a
    // real interoperability trap, not pedantry.
    let response = raw_handshake(server.local_addr, Some("permessage-deflate")).await;
    assert!(response.starts_with("HTTP/1.1 101"), "{response}");
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("client_max_window_bits"),
        "must not name a parameter the client did not offer:\n{response}"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deflate_can_still_be_declined_by_configuration() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture not present");
        return;
    };
    // The M4 finding — that real clients carry on when the extension is
    // declined — is what made shipping uncompressed viable, and it stays a
    // supported configuration for debugging a wire capture.
    let config = NetConfig {
        deflate: pahoa_net::ws::handshake::DeflateConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let server = start_with(data, config).await;

    let response = raw_handshake(
        server.local_addr,
        Some("permessage-deflate; client_max_window_bits"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 101"), "{response}");
    // Declining means saying nothing about it. Echoing an extension we will not
    // perform would make the client compress frames we then fail to read.
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("sec-websocket-extensions"),
        "a declined extension must be omitted, not echoed:\n{response}"
    );

    server.shutdown().await;
}
