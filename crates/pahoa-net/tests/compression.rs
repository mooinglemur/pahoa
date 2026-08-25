//! M8's exit gate: a broadcast is compressed once, not once per connection.
//!
//! This is the number the whole milestone exists for. With context takeover a
//! compressor carries its window between messages, so the same payload
//! compresses to *different bytes* for every connection and one broadcast costs
//! one compression per recipient — 17 million of them across a 2000-slot mass
//! release at 6000 connections. Declaring `server_no_context_takeover` makes
//! compression a pure function of the payload, so a shard compresses once and
//! hands the same `Bytes` to everyone it serves.
//!
//! The test connects many clients and counts. Correct behavior is bounded by
//! the *shard* count; the failure mode is bounded by the *connection* count,
//! and the two are far enough apart to be unambiguous.

use pahoa_multidata::MultiData;
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";
const SHARDS: usize = 4;
const CLIENTS: usize = 64;

/// Serializes the two tests in this file.
///
/// Both measure `pahoa_net::ws::deflate::compressions()`, which is a
/// **process-wide** counter, by sampling it around an action. Cargo runs the
/// tests in one binary concurrently, so without this the other test's
/// compressions land inside the measurement window — reliably enough under a
/// loaded machine to fail about one full-workspace run in three, and never when
/// this file is run on its own, which is the worst way for a flake to behave.
/// Async-aware rather than `std`, because the guard is held across the awaits
/// that do the measuring — a blocking lock there would park a runtime worker.
///
/// **Taken before the setup, not just around the measurement.** Holding it only
/// over the sampling window is not enough and was still failing under load: the
/// *other* test's setup connects 64 deflate clients, each join is an `AllText`
/// broadcast, and each of those compresses. Those compressions land inside
/// whatever window the other test is measuring, however tight it is. Serializing
/// the tests whole is the only version of this that holds.
static COUNTER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn load() -> Option<Arc<MultiData>> {
    let dir = std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crate is two levels below the workspace root")
                .join("crates/pahoa-pickle/tests/fixtures")
        });
    let raw = std::fs::read(dir.join(FIXTURE)).ok()?;
    Some(Arc::new(MultiData::parse(&raw).expect("fixture parses")))
}

/// A client that negotiates deflate but never decodes anything.
///
/// It only has to *exist* and be counted as a deflate recipient — what is being
/// measured happens on the server, before a byte reaches the socket. Building
/// it by hand also keeps the test independent of any client library, none of
/// which implement permessage-deflate anyway.
struct RawClient {
    stream: TcpStream,
}

impl RawClient {
    async fn connect(addr: std::net::SocketAddr, deflate: bool) -> Self {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let extensions = if deflate {
            "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n"
        } else {
            ""
        };
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             {extensions}\r\n"
        );
        stream.write_all(request.as_bytes()).await.expect("write");

        // Read exactly the handshake response, leaving any frames behind.
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
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 101"), "{text}");
        assert_eq!(
            text.to_ascii_lowercase().contains("permessage-deflate"),
            deflate,
            "unexpected extension negotiation:\n{text}"
        );

        Self { stream }
    }

    /// Send a masked text frame, as a client must (RFC 6455 §5.1).
    async fn send(&mut self, text: &str) {
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
        self.stream.write_all(&out).await.expect("send");
    }

    /// Drain whatever has arrived, so the server's outbound queue keeps moving.
    async fn drain(&mut self) {
        let _ = self.drain_bytes().await;
    }

    /// The same, keeping the bytes so a caller can inspect the framing.
    async fn drain_bytes(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(50), self.stream.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }
}

/// Walk server-to-client frames, yielding `(rsv1, opcode)` for each complete
/// one.
///
/// RSV1 is the permessage-deflate bit (RFC 7692 §7.2.3.1), and reading it off
/// the wire is the only way to check the property that matters: a client which
/// did not negotiate the extension cannot decode a frame carrying it, so
/// sending one is a broken connection rather than a wasted CPU cycle. Server
/// frames are never masked, which keeps this walk short.
fn frames(bytes: &[u8]) -> Vec<(bool, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let rsv1 = bytes[i] & 0x40 != 0;
        let opcode = bytes[i] & 0x0f;
        let masked = bytes[i + 1] & 0x80 != 0;
        let mut len = (bytes[i + 1] & 0x7f) as usize;
        let mut header = 2;
        if len == 126 {
            if i + 4 > bytes.len() {
                break;
            }
            len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            header = 4;
        } else if len == 127 {
            if i + 10 > bytes.len() {
                break;
            }
            len =
                u64::from_be_bytes(bytes[i + 2..i + 10].try_into().expect("eight bytes")) as usize;
            header = 10;
        }
        if masked {
            header += 4;
        }
        if i + header + len > bytes.len() {
            break;
        }
        out.push((rsv1, opcode));
        i += header + len;
    }
    out
}

fn connect_packet(name: &str, game: &str) -> String {
    serde_json::json!([{
        "cmd": "Connect",
        "password": null,
        "game": game,
        "name": name,
        "uuid": "compression-test",
        "version": {"major": 0, "minor": 6, "build": 8, "class": "Version"},
        "items_handling": 0,
        "tags": ["AP"],
        "slot_data": false,
    }])
    .to_string()
}

/// Long enough for the room to accept it; the admin surface is here only so
/// these tests can read their own metrics back.
const TOKEN: &str = "test-token-of-at-least-thirty-two-bytes";

/// Scrape `/admin/v1/metrics` over a plain socket.
async fn admin_metrics(server: &Server) -> String {
    let mut stream = TcpStream::connect(server.local_addr)
        .await
        .expect("connect");
    stream
        .write_all(
            format!(
                "GET /admin/v1/metrics HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out)
        .split_once("\r\n\r\n")
        .expect("a complete response")
        .1
        .to_string()
}

async fn start(data: Arc<MultiData>) -> Server {
    let (names, _) = data.resolve_datapackage();
    let room = Room::new(data, Arc::new(names), RoomOptions::default(), 0.0);
    Server::start(
        room,
        NetConfig {
            port: 0,
            shards: Some(SHARDS),
            admin_token: Some(TOKEN.to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("binds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broadcast_is_compressed_once_per_shard_not_once_per_connection() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    // Held across the *whole* test, setup included. See `COUNTER`.
    let _exclusive = COUNTER.lock().await;
    let slots: Vec<(String, String)> = data
        .player_slots()
        .take(CLIENTS)
        .map(|(_, info)| (info.name.clone(), info.game.clone()))
        .collect();
    assert!(
        slots.len() >= CLIENTS,
        "fixture has only {} player slots",
        slots.len()
    );

    let server = start(data).await;

    let mut clients = Vec::new();
    for (name, game) in &slots {
        let mut client = RawClient::connect(server.local_addr, true).await;
        client.send(&connect_packet(name, game)).await;
        clients.push(client);
    }
    // Let every join settle, and empty the sockets so nothing is blocked on
    // backpressure when the measured broadcast goes out.
    for client in &mut clients {
        client.drain().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for client in &mut clients {
        client.drain().await;
    }

    // One `Say` is exactly one `Recipients::AllText` broadcast.
    let before = pahoa_net::ws::deflate::compressions();
    clients[0]
        .send(&serde_json::json!([{"cmd": "Say", "text": "hello everyone"}]).to_string())
        .await;
    for client in &mut clients {
        client.drain().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for client in &mut clients {
        client.drain().await;
    }
    let compressions = pahoa_net::ws::deflate::compressions() - before;

    eprintln!(
        "one broadcast to {CLIENTS} deflate connections across {SHARDS} shards: \
         {compressions} compressions"
    );

    // The claim. A shard compresses once for all of its own recipients, so the
    // ceiling is the shard count however many connections there are. Per
    // connection would be 64 here and 6000 in production.
    assert!(
        compressions <= SHARDS as u64,
        "a broadcast to {CLIENTS} connections cost {compressions} compressions; \
         it should cost at most {SHARDS}, one per shard — \
         server_no_context_takeover is what makes the result shareable"
    );
    assert!(
        compressions >= 1,
        "nothing was compressed, so this measured nothing"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_without_deflate_costs_no_compression_at_all() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let _exclusive = COUNTER.lock().await;
    let (name, game) = data
        .player_slots()
        .next()
        .map(|(_, i)| (i.name.clone(), i.game.clone()))
        .expect("fixture has players");
    let server = start(data).await;

    // A room where nobody negotiated the extension must never invoke the
    // compressor — the deflated variant is built lazily, only when a shard
    // actually has a recipient for it.
    let mut client = RawClient::connect(server.local_addr, false).await;
    client.send(&connect_packet(&name, &game)).await;
    client.drain().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let before = pahoa_net::ws::deflate::compressions();
    client
        .send(&serde_json::json!([{"cmd": "Say", "text": "nobody wants deflate"}]).to_string())
        .await;
    client.drain().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.drain().await;

    assert_eq!(
        pahoa_net::ws::deflate::compressions() - before,
        0,
        "compressed for a connection that never asked for it"
    );

    server.shutdown().await;
}

/// **The negotiation is honored per connection, checked on the wire.**
///
/// The test above proves the compressor is never *invoked* for a room with no
/// deflate recipients. This is the sharper question, and the one that breaks a
/// client rather than wasting a cycle: with a deflate connection open — so the
/// compressed variant of every broadcast exists and is sitting in the shard's
/// memo — does the plain connection still get the plain one? A client that did
/// not negotiate the extension cannot decode an RSV1 frame at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broadcast_reaches_each_connection_in_the_form_it_negotiated() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    // This test compresses, and the other two measure the process-wide
    // compression counter by sampling it around an action — so it has to be
    // serialized against them for the same reason they are against each other.
    // See `COUNTER`.
    let _exclusive = COUNTER.lock().await;
    let players: Vec<(String, String)> = data
        .player_slots()
        .take(2)
        .map(|(_, i)| (i.name.clone(), i.game.clone()))
        .collect();
    assert_eq!(players.len(), 2, "the fixture needs two players");
    let server = start(data).await;

    // Taken before anyone connects: the deflate table is process-wide, and the
    // other tests in this binary connect 64 clients against these same fixture
    // slots. Deltas are the only reading immune to which test ran first.
    let baseline = admin_metrics(&server).await;

    let mut squeezed = RawClient::connect(server.local_addr, true).await;
    let mut plain = RawClient::connect(server.local_addr, false).await;
    squeezed
        .send(&connect_packet(&players[0].0, &players[0].1))
        .await;
    plain
        .send(&connect_packet(&players[1].0, &players[1].1))
        .await;
    squeezed.drain().await;
    plain.drain().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    squeezed.drain().await;
    plain.drain().await;

    // Long and repetitive, so it is over the 128-byte floor and compresses to
    // well under the original — a short line is sent plain to everyone and
    // would make both halves of this pass for the wrong reason.
    let chat = "deflate ".repeat(64);
    squeezed
        .send(&serde_json::json!([{"cmd": "Say", "text": chat}]).to_string())
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let squeezed_frames = frames(&squeezed.drain_bytes().await);
    let plain_frames = frames(&plain.drain_bytes().await);

    assert!(
        !plain_frames.is_empty() && !squeezed_frames.is_empty(),
        "both connections must have received something, or this proves nothing: \
         {squeezed_frames:?} / {plain_frames:?}"
    );
    assert!(
        squeezed_frames.iter().any(|(rsv1, _)| *rsv1),
        "the connection that negotiated deflate should have been sent a compressed \
         frame: {squeezed_frames:?}"
    );
    assert!(
        plain_frames.iter().all(|(rsv1, _)| !*rsv1),
        "a connection that never negotiated permessage-deflate was sent an RSV1 frame \
         it cannot decode: {plain_frames:?}"
    );

    // And the metric says the same thing the wire does. Two clients, two slots,
    // one compressing and one not — which is the correlation the metric exists
    // to expose, and it can only be built where the handshake's answer and the
    // slot's game are both in hand.
    let body = admin_metrics(&server).await;
    let count = |text: &str, player: &str, on: bool| -> u64 {
        text.lines()
            .find(|l| {
                l.starts_with("pahoa_client_connections_total{")
                    && l.contains(&format!(r#"player="{player}""#))
                    && l.contains(&format!(r#"deflate="{on}""#))
            })
            .and_then(|l| l.rsplit(' ').next()?.parse().ok())
            .unwrap_or(0)
    };
    let grew = |player: &str, on: bool| count(&body, player, on) - count(&baseline, player, on);

    assert_eq!(
        grew(&players[0].0, true),
        1,
        "the deflate client should be counted as compressing:\n{body}"
    );
    assert_eq!(
        grew(&players[1].0, false),
        1,
        "and the plain one as not:\n{body}"
    );
    assert_eq!(
        (grew(&players[0].0, false), grew(&players[1].0, true)),
        (0, 0),
        "neither should also appear under the other answer:\n{body}"
    );

    server.shutdown().await;
}
