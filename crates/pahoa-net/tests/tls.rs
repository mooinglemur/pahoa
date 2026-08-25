//! TLS termination on the room port, end to end over real sockets.
//!
//! The multidata here is synthetic rather than a fixture. These tests are about
//! the transport and not the game, and building the seed in-process is what
//! lets them run in CI, where the `.archipelago` fixtures are absent by design.

use pahoa_multidata::{LocationStore, MultiData, Version};
use pahoa_net::{NetConfig, Server, TlsPaths};
use pahoa_room::{Room, RoomOptions};
use rustls::pki_types::CertificateDer;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A WebSocket upgrade request, byte for byte what a client sends.
const UPGRADE: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
    Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
    Sec-WebSocket-Version: 13\r\n\r\n";

/// The opening bytes of a real ClientHello: handshake record, legacy version,
/// record length, then the handshake header.
const CLIENT_HELLO: &[u8] = &[0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xfc];

/// An ordinary request — a person with `curl`, a probe, a browser — as opposed
/// to [`UPGRADE`]. The two are refused differently and that is the point.
const PLAIN_GET: &[u8] = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n";

/// A room with nothing in it. Enough to accept connections and answer RoomInfo.
fn empty_room() -> Room {
    let data = Arc::new(MultiData {
        seed_name: "tls-test".to_string(),
        generator_version: Version::new(0, 6, 2),
        minimum_server_version: Version::new(0, 1, 6),
        minimum_client_versions: HashMap::new(),
        slot_info: BTreeMap::new(),
        connect_names: HashMap::new(),
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

/// A scratch directory standing in for the mounted Secret.
struct Mount(PathBuf);

impl Mount {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pahoa-tlsit-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        Self(dir)
    }

    /// Write a self-signed pair for `localhost`, returning the paths and the
    /// leaf DER for the client to trust.
    fn issue(&self) -> (TlsPaths, CertificateDer<'static>) {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("a self-signed certificate");
        let paths = TlsPaths {
            cert: self.0.join("tls.crt"),
            key: self.0.join("tls.key"),
        };
        std::fs::write(&paths.cert, issued.cert.pem()).unwrap();
        std::fs::write(&paths.key, issued.signing_key.serialize_pem()).unwrap();
        (paths, issued.cert.der().clone())
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn start(tls: Option<TlsPaths>, allow_plaintext: bool) -> Server {
    Server::start(
        empty_room(),
        NetConfig {
            port: 0,
            tls,
            allow_plaintext,
            ..Default::default()
        },
    )
    .await
    .expect("server should bind")
}

fn connector(root: CertificateDer<'static>) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(root)
        .expect("the test certificate is a valid root");
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Send `request` over a bare TCP connection and read whatever comes back.
async fn plaintext_exchange(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).await.expect("connects");
    stream.write_all(request).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    // The server closes after answering these, so a read to end terminates.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    buf
}

#[tokio::test]
async fn a_wss_client_upgrades_and_is_greeted() {
    let mount = Mount::new("wss");
    let (paths, root) = mount.issue();
    let server = start(Some(paths), false).await;

    let tcp = TcpStream::connect(server.local_addr)
        .await
        .expect("connects");
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = connector(root)
        .connect(name, tcp)
        .await
        .expect("the TLS handshake should complete");

    // The WebSocket handshake rides on top, exactly as it does in plaintext.
    let (mut ws, response) = tokio_tungstenite::client_async("ws://localhost/", tls)
        .await
        .expect("the upgrade should be accepted");
    assert_eq!(response.status(), 101);

    // And the room is really behind it: RoomInfo is unprompted, on connect.
    use futures_util::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("RoomInfo should arrive")
        .expect("a message")
        .expect("not an error");
    let text = first.into_text().expect("a text frame");
    assert!(
        text.contains("RoomInfo"),
        "expected RoomInfo over wss, got {text}"
    );

    server.shutdown().await;
}

/// **pahoa's own WebSocket client, over TLS, negotiating deflate.**
///
/// The test above uses `tokio-tungstenite`, which is fine for proving the room
/// answers — but tungstenite has no permessage-deflate at all and rejects any
/// frame with RSV1 set, so it cannot exercise the one thing this client exists
/// for. Until `Client` became generic over its stream it opened its own
/// `TcpStream`, which meant the load driver could reach a plaintext room and
/// nothing else: pahoa's TLS listener had never been driven by pahoa's own
/// harness, and puna's load generator could not reach a real room at all,
/// because every one of theirs is `wss://`.
///
/// So this is the combination that was unreachable: a caller-supplied TLS
/// stream, a `Host` that is a name rather than a socket address, and the
/// extension actually negotiated.
#[tokio::test]
async fn pahoas_own_client_speaks_deflate_over_a_caller_supplied_tls_stream() {
    use pahoa_net::ws::client::Client;

    let mount = Mount::new("client-tls");
    let (paths, root) = mount.issue();
    let server = start(Some(paths), false).await;

    let tcp = TcpStream::connect(server.local_addr)
        .await
        .expect("connects");
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = connector(root)
        .connect(name, tcp)
        .await
        .expect("the TLS handshake should complete");

    // The caller owns SNI and verification above; this owns only the upgrade.
    let mut client = Client::handshake(tls, "localhost", true)
        .await
        .expect("the upgrade should be accepted over TLS");

    assert!(
        client.deflate,
        "the whole point of this client is that it negotiates the extension"
    );

    let first = client
        .recv()
        .await
        .expect("a message")
        .expect("not a close");
    assert!(
        first.contains("RoomInfo"),
        "expected RoomInfo over wss, got {first}"
    );

    // And it inflates what comes back: `RoomInfo` on a real seed is well past
    // the server's 128-byte compression floor, so that text arrived as a
    // compressed frame and came out of the inflater intact.
    assert!(
        first.len() > 128,
        "a payload short enough to be sent plain would not prove inflation: {first}"
    );

    // Splitting works over a non-`TcpStream` too, which is what a load driver
    // needs to receive continuously while it sends.
    let (mut reader, mut writer) = client.split();
    writer
        .send(&serde_json::json!([{"cmd": "Sync"}]).to_string())
        .await
        .expect("sends over TLS");
    let _ = tokio::time::timeout(Duration::from_secs(5), reader.recv()).await;

    server.shutdown().await;
}

/// An ordinary plaintext request gets RFC 2817's status, which is the legible
/// answer for the human or the probe that sent it.
#[tokio::test]
async fn a_plaintext_http_request_is_refused_with_426() {
    let mount = Mount::new("refuse");
    let (paths, _) = mount.issue();
    let server = start(Some(paths), false).await;

    let response = plaintext_exchange(server.local_addr, PLAIN_GET).await;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 426 Upgrade Required"),
        "got {response:?}"
    );
    // RFC 2817: name what to upgrade to, so the refusal is actionable.
    assert!(
        response.contains("Upgrade: TLS/1.3, HTTP/1.1"),
        "got {response:?}"
    );

    server.shutdown().await;
}

/// **A plaintext WebSocket upgrade gets no reply at all, deliberately.**
///
/// Archipelago clients are handed a bare `host:port` and try `ws://` first
/// (`CommonClient.py:857`). They recover through one narrow heuristic: the
/// `websockets` library raises `InvalidMessage` when the reply is not parseable
/// HTTP, and `CommonClient.py:887-890` reads that as "probably encrypted" and
/// retries as `wss://`. A room behind an ordinary TLS terminator produces alert
/// bytes, so the retry fires and the player never notices.
///
/// A well-formed `426` defeats it — `websockets` parses that happily and raises
/// `InvalidStatusCode`, which is not the branch that retries. Sending the
/// correct status therefore stranded clients that the reference's accidental
/// behavior would have connected, which is how this was found: Universal
/// Tracker reporting a 426 against a live room.
///
/// So the upgrade path closes without answering, and only the request path
/// above gets the status.
#[tokio::test]
async fn a_plaintext_websocket_upgrade_is_closed_on_so_the_client_retries_over_tls() {
    let mount = Mount::new("refuse-ws");
    let (paths, _) = mount.issue();
    let server = start(Some(paths), false).await;

    let response = plaintext_exchange(server.local_addr, UPGRADE).await;
    assert!(
        response.is_empty(),
        "a ws:// client must see an unparseable (empty) response so its \
         wss:// retry fires; got {:?}",
        String::from_utf8_lossy(&response)
    );

    server.shutdown().await;
}

#[tokio::test]
async fn allow_plaintext_keeps_the_unencrypted_port_working() {
    let mount = Mount::new("allow");
    let (paths, _) = mount.issue();
    let server = start(Some(paths), true).await;

    let response = plaintext_exchange(server.local_addr, UPGRADE).await;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 101 Switching Protocols"),
        "got {response:?}"
    );

    server.shutdown().await;
}

/// The behavior that predates TLS and must survive it: a client probing
/// `wss://` against a plaintext room gets a fatal alert immediately, rather
/// than sitting on the handshake timeout waiting for a ServerHello.
#[tokio::test]
async fn a_client_hello_without_a_certificate_still_gets_a_fatal_alert() {
    let server = start(None, false).await;

    let started = std::time::Instant::now();
    let response = plaintext_exchange(server.local_addr, CLIENT_HELLO).await;
    assert_eq!(response, [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}, so it waited on the handshake timeout",
        started.elapsed()
    );

    server.shutdown().await;
}

#[tokio::test]
async fn plaintext_still_upgrades_when_no_certificate_is_configured() {
    let server = start(None, false).await;
    let response = plaintext_exchange(server.local_addr, UPGRADE).await;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 101 Switching Protocols"),
        "got {response:?}"
    );
    server.shutdown().await;
}

/// puna binds `::` rather than `0.0.0.0`, because the cluster's Services are
/// v6-capable and a v4-only listener answers a v6 connect with an instant RST.
/// That this parses *and* still accepts v4-mapped connections was listed as
/// unverified in the handoff; this is the verification.
#[tokio::test]
async fn the_v6_wildcard_accepts_a_v4_mapped_connection() {
    let started = Server::start(
        empty_room(),
        NetConfig {
            bind: "::".to_string(),
            port: 0,
            ..Default::default()
        },
    )
    .await;

    // A container with IPv6 switched off is a skip, not a failure.
    let Ok(server) = started else {
        eprintln!("SKIP: cannot bind [::] here");
        return;
    };
    assert!(server.local_addr.is_ipv6(), "should be a v6 listener");

    let v4: SocketAddr = ([127, 0, 0, 1], server.local_addr.port()).into();
    let response = plaintext_exchange(v4, UPGRADE).await;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 101 Switching Protocols"),
        "a v4-mapped connection should be served, got {response:?}"
    );

    server.shutdown().await;
}
