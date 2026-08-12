//! How the server answers a client that offers `permessage-deflate`.
//!
//! This de-risks M8. The plan's open question is whether pahoa can ship Phase 1
//! uncompressed, which depends on clients tolerating a *declined* extension.
//! Two halves to that, and only one is ours:
//!
//! - the server must decline correctly — omit the extension from its handshake
//!   response, per RFC 6455 §9.1, rather than echoing something it cannot do
//! - real clients must then carry on, which only a real client can answer
//!
//! This covers our half. The Python `websockets` library that Archipelago's
//! client uses offers deflate by default, so this is exactly the handshake it
//! will perform.

use pahoa_multidata::{GamePackage, MultiData};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

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
    let snapshot: BTreeMap<String, GamePackage> = BTreeMap::new();
    let (names, _) = data.resolve_datapackage(&snapshot);
    let room = Room::new(data, Arc::new(names), RoomOptions::default(), 0.0);
    Server::start(
        room,
        NetConfig {
            port: 0,
            ..Default::default()
        },
    )
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
async fn permessage_deflate_is_declined_rather_than_echoed() {
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
        "upgrade must still succeed:\n{response}"
    );

    // Declining means saying nothing about it. Echoing an extension we cannot
    // perform would make the client compress frames we would then fail to read
    // — worse than not supporting it at all.
    let lower = response.to_ascii_lowercase();
    assert!(
        !lower.contains("sec-websocket-extensions"),
        "server must not accept an extension it does not implement:\n{response}"
    );

    server.shutdown().await;
}
