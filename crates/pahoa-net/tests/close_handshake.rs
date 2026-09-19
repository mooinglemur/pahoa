//! A client that closes politely must be answered politely.
//!
//! RFC 6455 §5.5.1: on receiving a close frame an endpoint sends one back, and
//! only then closes the socket. The reference server gets this for free from
//! `websockets`, which runs the closing handshake inside the library and does
//! not hand the connection back to `MultiServer` until the echo has been
//! written.
//!
//! pahoa's writer is a separate task, so the echo is *queued* rather than
//! written, and the reader then has to be careful not to tear the connection
//! down before the writer has drained it. It was not careful, and the symptom
//! was reported from the other end: `CommonClient` calls `socket.close()` on
//! `/disconnect`, `websockets` waits for the echo that never came, and the
//! ordinary end of a session was logged as a lost connection.
//!
//! Raw sockets rather than a client library, because a library hides exactly
//! the thing under test: it will report a tidy close for a peer that merely
//! hung up.

use pahoa_multidata::{LocationStore, MultiData, NetworkSlot, SlotType, Version};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn room() -> Room {
    let mut slot_info = BTreeMap::new();
    let mut connect_names = HashMap::new();
    slot_info.insert(
        1,
        NetworkSlot {
            name: "Troy".to_string(),
            game: "Archipelago".to_string(),
            slot_type: SlotType::Player,
            group_members: Vec::new(),
        },
    );
    connect_names.insert("Troy".to_string(), (0, 1));

    let data = Arc::new(MultiData {
        seed_name: "56807069331869547085".to_string(),
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
    Room::new(
        data,
        Arc::new(names),
        RoomOptions::default(),
        1_700_000_000.0,
    )
}

async fn start() -> Server {
    Server::start(
        room(),
        NetConfig {
            port: 0,
            ..Default::default()
        },
    )
    .await
    .expect("binds")
}

/// Mask a payload as a client must, under any opcode.
fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let key = [0x12u8, 0x34, 0x56, 0x78];
    let mut out = vec![0x80 | opcode];
    assert!(payload.len() < 126, "no long frames needed here");
    out.push(0x80 | payload.len() as u8);
    out.extend_from_slice(&key);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    out
}

async fn handshake(addr: SocketAddr) -> TcpStream {
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
            .expect("the handshake should not hang")
            .expect("read");
        assert_ne!(n, 0, "server closed during the handshake");
        response.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"));
    stream
}

/// Read until the peer stops talking, which after a close is imminent either
/// way: the question this answers is what arrived before the EOF.
async fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
        }
    }
    out
}

/// The server's frames, as `(opcode, payload)`. Server frames are never masked,
/// and nothing here is long enough to need the extended length.
fn parse(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while bytes.len() >= 2 {
        let opcode = bytes[0] & 0x0f;
        assert_eq!(bytes[1] & 0x80, 0, "a server frame must not be masked");
        let (len, header) = match bytes[1] & 0x7f {
            126 => (u16::from_be_bytes([bytes[2], bytes[3]]) as usize, 4),
            127 => panic!("no frame here should need a 64-bit length"),
            n => (n as usize, 2),
        };
        if bytes.len() < header + len {
            break;
        }
        out.push((opcode, bytes[header..header + len].to_vec()));
        bytes = &bytes[header + len..];
    }
    out
}

const CLOSE: u8 = 0x8;

/// The bug as the client saw it: `websockets` raises "no close frame received"
/// and `CommonClient` reports an ordinary `/disconnect` as a lost connection.
#[tokio::test]
async fn a_polite_close_is_answered_with_a_close() {
    let server = start().await;
    let mut stream = handshake(server.local_addr).await;

    // 1000, "going away" as a client spells it, is what `socket.close()` sends.
    let mut payload = 1000u16.to_be_bytes().to_vec();
    payload.extend_from_slice(b"connection closed");
    stream
        .write_all(&frame(CLOSE, &payload))
        .await
        .expect("send the close");

    let tail = read_to_end(&mut stream).await;
    let frames = parse(&tail);
    let close = frames.iter().find(|(opcode, _)| *opcode == CLOSE);
    let close = close.unwrap_or_else(|| {
        panic!(
            "no close frame came back before EOF; the peer got {} frame(s): {:?}",
            frames.len(),
            frames.iter().map(|(op, _)| op).collect::<Vec<_>>()
        )
    });

    assert!(close.1.len() >= 2, "a close code is two bytes or none");
    let code = u16::from_be_bytes([close.1[0], close.1[1]]);
    assert_eq!(code, 1000, "the peer's own code is echoed back");

    server.shutdown().await;
}

/// The same, for a client that got as far as playing. The authenticated path
/// has a populated outbound queue behind it, so it is the one where an echo
/// queued and never drained is easiest to lose.
#[tokio::test]
async fn a_connected_client_is_answered_too() {
    let server = start().await;
    let mut stream = handshake(server.local_addr).await;

    let connect = serde_json::json!([{
        "cmd": "Connect", "password": null, "game": "Archipelago", "name": "Troy",
        "uuid": "close", "version": {"major": 0, "minor": 6, "build": 8, "class": "Version"},
        "items_handling": 0, "tags": ["AP"], "slot_data": false,
    }])
    .to_string();
    stream
        .write_all(&{
            // The Connect packet is longer than 125 bytes, so it needs the
            // 16-bit length the helper above deliberately does not do.
            let payload = connect.as_bytes();
            let key = [0x12u8, 0x34, 0x56, 0x78];
            let mut out = vec![0x81, 0x80 | 126];
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            out.extend_from_slice(&key);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
            out
        })
        .await
        .expect("send Connect");

    // Let the room answer, so there is traffic on the queue before the close.
    tokio::time::sleep(Duration::from_millis(100)).await;

    stream
        .write_all(&frame(CLOSE, &1000u16.to_be_bytes()))
        .await
        .expect("send the close");

    let frames = parse(&read_to_end(&mut stream).await);
    assert!(
        frames.iter().any(|(opcode, _)| *opcode == CLOSE),
        "a connected client's close went unanswered; got {:?}",
        frames.iter().map(|(op, _)| op).collect::<Vec<_>>()
    );

    server.shutdown().await;
}
