//! What a keepalive timeout says about *who* stopped answering.
//!
//! The line an operator sees for this used to be `conn16131` and nothing more,
//! and no other line in the room paired a `ConnId` with a slot — so "did a real
//! player just drop, or was that a port scan?" was unanswerable from the log.
//!
//! It is unanswerable for a structural reason rather than an oversight. The
//! keepalive lives in the per-connection **writer** task, which owns a socket
//! half and a `ConnId`; the slot is the actor's, and the actor is not told why
//! a connection ended. This pins the seam that now joins them: the writer
//! reports its reason, the actor logs it against the slot, and it reaches the
//! journal — where an organizer reading a history months later can tell a
//! player who quit from a player whose connection kept dying.
//!
//! Driven over real sockets with a raw client, because "never answers a ping"
//! is not a state a WebSocket library will hold for you: every one of them
//! pongs automatically.

use pahoa_multidata::{LocationStore, MultiData, NetworkSlot, SlotType, Version};
use pahoa_net::actor::SaveConfig;
use pahoa_net::journal::{FILE_NAME, Journal};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Short enough that the test finishes, long enough that a loaded machine
/// cannot trip it before the client has authenticated. Detection takes up to
/// `interval + timeout`.
const PING_INTERVAL: Duration = Duration::from_millis(150);
const PING_TIMEOUT: Duration = Duration::from_millis(150);

fn room() -> Room {
    let mut slot_info = BTreeMap::new();
    let mut connect_names = HashMap::new();
    slot_info.insert(
        1,
        NetworkSlot {
            name: "P1".to_string(),
            game: "Archipelago".to_string(),
            slot_type: SlotType::Player,
            group_members: Vec::new(),
        },
    );
    slot_info.insert(
        2,
        NetworkSlot {
            name: "P2".to_string(),
            game: "Archipelago".to_string(),
            slot_type: SlotType::Player,
            group_members: Vec::new(),
        },
    );
    connect_names.insert("P1".to_string(), (0, 1));
    connect_names.insert("P2".to_string(), (0, 2));
    let data = Arc::new(MultiData {
        seed_name: "keepalive".to_string(),
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

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pahoa-keepalive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Mask a text frame the way a client must.
fn mask(text: &str) -> Vec<u8> {
    let payload = text.as_bytes();
    let mut frame = vec![0x81];
    // A `Connect` is comfortably over 125 bytes, so the extended length is not
    // optional here.
    assert!(payload.len() < 65536, "test payloads are not that big");
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    // A zero mask key, so the payload is its own masked form.
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(payload);
    frame
}

/// A masked, empty Pong.
///
/// Reading the socket is *not* enough to stay alive, which is the trap this
/// file fell into: the server counts pongs, and only a client sends those. It
/// does not care what the payload is — "only one ping is ever outstanding, so
/// any pong clears it" — so this need not echo anything.
const PONG: [u8; 6] = [0x8A, 0x80, 0, 0, 0, 0];

/// Open a socket, upgrade it, and optionally authenticate — then hand it back
/// without reading a byte, which is what makes the peer look dead.
async fn connect(addr: std::net::SocketAddr, name: Option<&str>) -> TcpStream {
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

    if let Some(name) = name {
        let connect = serde_json::json!([{
            "cmd": "Connect", "password": null, "game": "Archipelago", "name": name,
            "uuid": "keepalive", "version": {"major": 0, "minor": 6, "build": 8, "class": "Version"},
            "items_handling": 0, "tags": ["AP"], "slot_data": false,
        }])
        .to_string();
        stream.write_all(&mask(&connect)).await.expect("write");
    }
    stream
}

/// Every `disconnected` record the run wrote.
fn disconnects(dir: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(body) = std::fs::read_to_string(dir.join(FILE_NAME)) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        // `type`, not `event` — the tag the writer actually emits. Reading the
        // wrong key made every assertion below vacuously true.
        .filter(|v| v["type"] == "disconnected")
        .collect()
}

/// The writer comes back with the server: it owns the thread that puts records
/// on disk, and dropping it without `finish()` means reading the file before
/// the tail has been written.
async fn start(dir: &std::path::Path) -> (Server, pahoa_net::journal::JournalWriter) {
    let room = room();
    let (journal, writer) =
        Journal::open(dir, room.multidata_arc(), Arc::clone(room.datapackage()))
            .expect("journal opens");
    let server = Server::start_with_saves(
        room,
        NetConfig {
            port: 0,
            ping_interval: PING_INTERVAL,
            ping_timeout: PING_TIMEOUT,
            ..Default::default()
        },
        SaveConfig {
            journal: Some(journal),
            ..Default::default()
        },
    )
    .await
    .expect("binds");
    (server, writer)
}

/// Stop the room, then wait for the journal thread to drain. The thread exits
/// when the last `Journal` handle drops, which is what `shutdown` awaits.
async fn stop(server: Server, writer: pahoa_net::journal::JournalWriter) {
    server.shutdown().await;
    writer.finish();
}

/// The whole point: an authenticated client that stops answering is recorded
/// **as that slot**, with the reason that identifies it.
#[tokio::test]
async fn a_slot_whose_peer_stops_answering_is_recorded_with_its_reason() {
    let dir = temp_dir("authed");
    let (server, writer) = start(&dir).await;

    // Held open and never read from, so no pong is ever sent.
    let _dead = connect(server.local_addr, Some("P1")).await;

    // `interval + timeout` to notice, plus room for a loaded machine.
    tokio::time::sleep(PING_INTERVAL + PING_TIMEOUT + Duration::from_millis(600)).await;
    stop(server, writer).await;

    let records = disconnects(&dir);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["slot"], 1, "the record must name the slot");
    assert_eq!(records[0]["team"], 0);
    assert_eq!(records[0]["player"], "P1");
    assert_eq!(
        records[0]["reason"], "no pong within the keepalive timeout",
        "without this a history cannot tell a player who quit from one whose \
         connection died: {records:?}"
    );
}

/// The other half, and the one that answers the operator's real question.
///
/// A socket that never authenticated has no slot to name, so it writes no
/// record at all — which is itself the answer. A keepalive timeout in the log
/// with nothing beside it in the journal was a scanner or a half-open socket,
/// not a player who lost their game.
///
/// **The authenticated client is what makes this mean anything.** Asserting an
/// absence alone would pass on a server whose keepalive never fires, or whose
/// journal never reaches disk — both of which this file has already been
/// through once. So one dead socket of each kind, and the run must produce
/// exactly the one record.
#[tokio::test]
async fn an_unauthenticated_peer_that_stops_answering_names_no_slot() {
    let dir = temp_dir("unauthed");
    let (server, writer) = start(&dir).await;

    let _anonymous = connect(server.local_addr, None).await;
    let _authed = connect(server.local_addr, Some("P1")).await;

    tokio::time::sleep(PING_INTERVAL + PING_TIMEOUT + Duration::from_millis(600)).await;
    stop(server, writer).await;

    let records = disconnects(&dir);
    assert_eq!(
        records.len(),
        1,
        "the anonymous socket wrote a record of its own: {records:?}"
    );
    assert_eq!(records[0]["slot"], 1, "{records:?}");
}

/// The control. Without it the tests above pass on a server that drops every
/// connection it has, for any reason or none.
///
/// Two clients on two slots, one reading and one not, so the run proves both
/// halves at once: the keepalive fired, and it fired only at the peer that had
/// stopped answering.
#[tokio::test]
async fn a_client_that_answers_its_pings_is_left_alone() {
    let dir = temp_dir("healthy");
    let (server, writer) = start(&dir).await;

    let mut healthy = connect(server.local_addr, Some("P1")).await;
    let _dead = connect(server.local_addr, Some("P2")).await;

    // Drain, and answer. Anything the server sends is either a ping or
    // something a real client would have read anyway, and a pong after each
    // read is what a library would have done for us.
    let deadline =
        tokio::time::Instant::now() + PING_INTERVAL + PING_TIMEOUT + Duration::from_millis(600);
    let mut sink = [0u8; 4096];
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(50), healthy.read(&mut sink)).await
            && n > 0
        {
            healthy.write_all(&PONG).await.expect("pong");
        }
    }
    stop(server, writer).await;

    let dropped: Vec<_> = disconnects(&dir)
        .into_iter()
        .filter(|r| r["reason"] == "no pong within the keepalive timeout")
        .collect();
    assert_eq!(
        dropped.len(),
        1,
        "the keepalive should have caught the silent client and nobody else: {dropped:?}"
    );
    assert_eq!(
        dropped[0]["slot"], 2,
        "the client that was reading its socket was dropped: {dropped:?}"
    );
}
