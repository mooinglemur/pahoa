//! A connection pahoa has given up on must find out it was given up on.
//!
//! The failure this guards against is the worst shape a disconnect can take:
//! the server forgets the client, the client believes it is still playing, and
//! neither can tell. It resolves only when the player notices their inputs have
//! no effect.
//!
//! It arises from a close that depends on the queue it is closing, and the
//! mechanism is pinned deterministically in `shard.rs`'s own tests — including
//! the subtle half, where the queue has room but its writer is wedged.
//!
//! What lives here is the end-to-end half: an *administrator's kick* against a
//! client that is not reading, driven through the real admin API over real
//! sockets. That is the case an operator reported — the API answered
//! "Disconnected 1 connection" while the client stayed connected.
//!
//! The equivalent end-to-end test for *lagging* is deliberately absent. It has
//! to fill a kernel receive buffer to reach the state, which is not something a
//! test can make happen reliably: at one point it passed one run in three, and
//! a flaky test asserting a correctness property is worse than none. The
//! deterministic version in `shard.rs` covers the same invariant.

use pahoa_multidata::{LocationStore, MultiData, NetworkSlot, SlotType, Version};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Small enough that one talkative neighbour overwhelms a client that is not
/// reading, which is what a real release does at scale.
const BUDGET: usize = 32 * 1024;
const TOKEN: &str = "test-token-of-at-least-thirty-two-bytes";

fn room() -> Room {
    let mut slot_info = BTreeMap::new();
    let mut connect_names = HashMap::new();
    for i in 1..=2u32 {
        slot_info.insert(
            i,
            NetworkSlot {
                name: format!("P{i}"),
                game: "Archipelago".to_string(),
                slot_type: SlotType::Player,
                group_members: Vec::new(),
            },
        );
        connect_names.insert(format!("P{i}"), (0, i));
    }
    let data = Arc::new(MultiData {
        seed_name: "lag".to_string(),
        generator_version: Version::new(0, 6, 2),
        minimum_server_version: Version::new(0, 1, 6),
        minimum_client_versions: HashMap::new(),
        slot_info,
        connect_names,
        locations: LocationStore::default(),
        precollected_items: HashMap::new(),
        precollected_hints: HashMap::new(),
        er_hint_data: HashMap::new(),
        spheres: Vec::new(),
        race_mode: false,
        slot_data: HashMap::new(),
        server_options: None,
        embedded_datapackage: BTreeMap::new(),
    });
    let (names, _) = data.resolve_datapackage();
    Room::new(data, Arc::new(names), RoomOptions::default(), 0.0)
}

async fn start() -> Server {
    start_with(NetConfig {
        port: 0,
        outbound_budget_bytes: BUDGET,
        per_connection_budget_bytes: BUDGET,
        admin_token: Some(TOKEN.to_string()),
        // Off, so a keepalive ping cannot appear in the middle of a test about
        // closing and be mistaken for traffic.
        ping_interval: Duration::ZERO,
        ..Default::default()
    })
    .await
}

async fn start_with(config: NetConfig) -> Server {
    Server::start(room(), config).await.expect("binds")
}

/// Kick a slot through the real admin API, since that is the path an operator
/// takes and the one that reported success while the client stayed connected.
async fn kick(addr: std::net::SocketAddr, slot: u32) -> String {
    let body = serde_json::json!({"command": "kick", "slot": slot}).to_string();
    let request = format!(
        "POST /admin/v1/command HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).await.expect("connects");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

/// A raw WebSocket client, so that "never reads" is expressible — no library
/// will let you hold a connection open while ignoring it.
async fn connect(addr: std::net::SocketAddr, name: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("handshake should not hang")
            .expect("read");
        assert_ne!(n, 0, "server closed during the handshake");
        response.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"));

    send(
        &mut stream,
        &serde_json::json!([{
            "cmd": "Connect", "password": null, "game": "Archipelago", "name": name,
            "uuid": "lag", "version": {"major": 0, "minor": 6, "build": 8, "class": "Version"},
            "items_handling": 0, "tags": ["AP"], "slot_data": false,
        }])
        .to_string(),
    )
    .await;
    stream
}

/// As [`send`], but reports failure instead of panicking.
///
/// The loud client receives its own broadcasts, so it can be dropped for lagging
/// too if it falls behind. That is not the thing under test, so it ends the loop
/// rather than the run.
async fn try_send(stream: &mut TcpStream, text: &str) -> bool {
    let frame = mask(text);
    stream.write_all(&frame).await.is_ok()
}

/// Drain everything currently readable, so this client stays healthy.
async fn drain(stream: &mut TcpStream) {
    let mut sink = [0u8; 65536];
    while let Ok(Ok(n)) =
        tokio::time::timeout(Duration::from_millis(20), stream.read(&mut sink)).await
    {
        if n == 0 {
            break;
        }
    }
}

fn mask(text: &str) -> Vec<u8> {
    let payload = text.as_bytes();
    let key = [0x12u8, 0x34, 0x56, 0x78];
    let mut out = vec![0x81];
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(&key);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    out
}

async fn send(stream: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    let key = [0x12u8, 0x34, 0x56, 0x78];
    let mut out = vec![0x81];
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(&key);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    stream.write_all(&out).await.expect("send");
}

/// The whole point: a socket the server has stopped tracking must not stay open.
///
/// "Closed" is read from the peer's side, because that is the only place the bug
/// is visible — the server's own bookkeeping said the connection was dropped
/// even while it was not.
async fn is_closed_by_peer(stream: &mut TcpStream) -> bool {
    let mut buf = [0u8; 65536];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buf)).await {
            // EOF, or a close frame followed by one.
            Ok(Ok(0)) => return true,
            // Anything else is the backlog the client never drained; keep
            // reading only to find the end of it.
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return true,
            Err(_) => {}
        }
    }
    false
}

/// The same guarantee for an administrator's kick.
///
/// A kick aimed at a struggling client is the most likely kind, and therefore
/// the one most likely to have silently done nothing — the admin API reported
/// "Disconnected 1 connection" while the client stayed connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kicked_client_is_disconnected_even_when_it_is_not_reading() {
    let server = start().await;

    let mut slow = connect(server.local_addr, "P1").await;
    let mut loud = connect(server.local_addr, "P2").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Back the victim's queue up first, so the close cannot travel on it.
    let filler = "y".repeat(4096);
    for _ in 0..40 {
        let line = serde_json::json!([{"cmd": "Say", "text": filler}]).to_string();
        if !try_send(&mut loud, &line).await {
            break;
        }
        drain(&mut loud).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let response = kick(server.local_addr, 1).await;
    assert!(
        response.contains("\"ok\":true"),
        "the kick itself failed: {response}"
    );

    assert!(
        is_closed_by_peer(&mut slow).await,
        "the kick reported success but the client is still connected"
    );

    server.shutdown().await;
}

// --- keepalives ----------------------------------------------------------

/// Read one frame's opcode, or `None` if nothing arrives in time.
async fn opcode(stream: &mut TcpStream, within: Duration) -> Option<u8> {
    let mut head = [0u8; 2];
    tokio::time::timeout(within, stream.read_exact(&mut head))
        .await
        .ok()?
        .ok()?;
    let opcode = head[0] & 0x0f;
    let len = (head[1] & 0x7f) as usize;
    if len > 0 {
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body).await;
    }
    Some(opcode)
}

/// The server must ping, because nothing else will.
///
/// Archipelago's own clients connect with `ping_interval=None`, so an idle
/// connection carries no traffic in either direction unless the server makes
/// some — and a middlebox that reaps idle flows will take it, telling neither
/// end. Observed in the wild between two clients on one machine: the one that
/// pinged survived, the one that did not was dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_pings_an_idle_connection() {
    let server = start_with(NetConfig {
        port: 0,
        ping_interval: Duration::from_millis(150),
        ping_timeout: Duration::from_secs(30),
        ..Default::default()
    })
    .await;

    let mut client = connect(server.local_addr, "P1").await;
    // Drain the join traffic, then say nothing at all.
    drain(&mut client).await;

    // Opcode 0x9 is Ping (RFC 6455 §5.5.2).
    let seen = opcode(&mut client, Duration::from_secs(5)).await;
    assert_eq!(
        seen,
        Some(0x9),
        "an idle connection received no ping, so nothing keeps it alive"
    );

    server.shutdown().await;
}

/// A peer that never answers is dropped, rather than holding its slot forever.
///
/// The pong is the only evidence available: writing a ping to a dead peer
/// *succeeds*, because the bytes land in the local send buffer and TCP retries
/// for minutes. Nothing else reports the death.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_never_pongs_is_dropped() {
    let server = start_with(NetConfig {
        port: 0,
        ping_interval: Duration::from_millis(100),
        ping_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await;

    // Connects, then answers nothing. Reading without replying is exactly a
    // client whose process has died with its socket still open.
    let mut mute = connect(server.local_addr, "P1").await;

    assert!(
        is_closed_by_peer(&mut mute).await,
        "a client that never answered a ping is still connected, so its slot \
         stays occupied by nobody"
    );

    server.shutdown().await;
}

/// Zero means off, and off must mean silent rather than "very fast".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_interval_disables_pinging_entirely() {
    let server = start_with(NetConfig {
        port: 0,
        ping_interval: Duration::ZERO,
        ping_timeout: Duration::from_millis(50),
        ..Default::default()
    })
    .await;

    let mut client = connect(server.local_addr, "P1").await;
    drain(&mut client).await;

    assert_eq!(
        opcode(&mut client, Duration::from_millis(600)).await,
        None,
        "pings were disabled but something still arrived"
    );

    server.shutdown().await;
}
