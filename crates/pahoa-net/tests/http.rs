//! The HTTP surface, over real sockets, on the same port as the game.
//!
//! Synthetic multidata rather than a fixture, so these run in CI — see
//! `tests/tls.rs` for the same reasoning.

use pahoa_multidata::{GamePackage, LocationStore, MultiData, NetworkSlot, SlotType, Version};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn slot(name: &str, game: &str) -> NetworkSlot {
    NetworkSlot {
        name: name.to_string(),
        game: game.to_string(),
        slot_type: SlotType::Player,
        group_members: Vec::new(),
    }
}

/// A two-slot room, enough for the public description to have something in it.
fn room(options: RoomOptions) -> Room {
    let mut slot_info = BTreeMap::new();
    slot_info.insert(1, slot("Troy", "A Link to the Past"));
    slot_info.insert(2, slot("Kai", "Super Metroid"));

    let mut connect_names = HashMap::new();
    connect_names.insert("Troy".to_string(), (0, 1));
    connect_names.insert("Kai".to_string(), (0, 2));

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

    let snapshot: BTreeMap<String, GamePackage> = BTreeMap::new();
    let (names, _) = data.resolve_datapackage(&snapshot);
    Room::new(data, Arc::new(names), options, 1_700_000_000.0)
}

async fn start(options: RoomOptions) -> Server {
    Server::start(
        room(options),
        NetConfig {
            port: 0,
            ..Default::default()
        },
    )
    .await
    .expect("server should bind")
}

/// A token long enough to be accepted, and recognizable in a failure message.
const TOKEN: &str = "test-token-of-at-least-thirty-two-bytes";

async fn start_with_admin() -> Server {
    Server::start(
        room(RoomOptions::default()),
        NetConfig {
            port: 0,
            admin_token: Some(TOKEN.to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("server should bind")
}

async fn authed(addr: SocketAddr, method: &str, path: &str, token: &str) -> String {
    request(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: 0\r\n\r\n"
        ),
    )
    .await
}

/// One request, one response, connection closed — which is what every response
/// on this surface does.
async fn request(addr: SocketAddr, raw: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connects");
    stream.write_all(raw.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn get(addr: SocketAddr, path: &str) -> String {
    request(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")).await
}

/// Split a response into its status line and its body.
///
/// Owned rather than borrowed so a caller can write `split(&get(..).await)`
/// without keeping the response alive by hand.
fn split(response: &str) -> (String, String) {
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    (
        head.lines().next().unwrap_or("").to_string(),
        body.to_string(),
    )
}

#[tokio::test]
async fn healthz_answers_once_the_listener_is_up() {
    let server = start(RoomOptions::default()).await;
    // Reaching this at all is the signal: the listener binds only after the
    // save has been restored, so there is no state to consult.
    let (status, body) = split(&get(server.local_addr, "/healthz").await);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body, "ok\n");
    server.shutdown().await;
}

#[tokio::test]
async fn the_public_room_description_carries_no_secrets() {
    let server = start(RoomOptions {
        password: Some("quiet-harbor-ledger".to_string()),
        server_password: Some("admin-secret".to_string()),
        ..Default::default()
    })
    .await;

    let response = get(server.local_addr, "/api/v1/room").await;
    let (status, body) = split(&response);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(response.contains("Content-Type: application/json"));

    let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(json["seed_name"], "56807069331869547085");
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["pahoa_version"], env!("CARGO_PKG_VERSION"));
    // That a password is needed is public; the password is not.
    assert_eq!(json["password"], true);
    assert!(
        !body.contains("quiet-harbor-ledger") && !body.contains("admin-secret"),
        "a secret reached the public surface: {body}"
    );

    let slots = json["slots"].as_array().expect("slots");
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0]["slot"], 1);
    assert_eq!(slots[0]["name"], "Troy");
    assert_eq!(slots[0]["game"], "A Link to the Past");

    server.shutdown().await;
}

/// The live count comes from the actor, so it is the one field that proves the
/// HTTP surface can reach room state at all.
#[tokio::test]
async fn the_room_description_counts_live_connections() {
    let server = start(RoomOptions::default()).await;

    let before = get(server.local_addr, "/api/v1/room").await;
    let before: serde_json::Value = serde_json::from_str(&split(&before).1).unwrap();
    assert_eq!(before["clients_connected"], 0);

    // Hold a socket open across the next request.
    let _held = TcpStream::connect(server.local_addr).await.unwrap();
    let ws = tokio_tungstenite::client_async(
        format!("ws://{}/", server.local_addr),
        TcpStream::connect(server.local_addr).await.unwrap(),
    )
    .await
    .expect("upgrades")
    .0;

    let during = get(server.local_addr, "/api/v1/room").await;
    let during: serde_json::Value = serde_json::from_str(&split(&during).1).unwrap();
    assert_eq!(
        during["clients_connected"], 1,
        "the upgraded connection should be counted"
    );

    drop(ws);
    server.shutdown().await;
}

/// The admin surface is *absent* rather than locked while no token is
/// configured, so a misconfiguration fails closed and looks like an old build.
#[tokio::test]
async fn the_admin_surface_is_absent_without_a_token() {
    let server = start(RoomOptions::default()).await;
    for path in ["/admin/v1/status", "/admin/v1/metrics", "/admin/v1/command"] {
        let (status, _) = split(&get(server.local_addr, path).await);
        assert_eq!(status, "HTTP/1.1 404 Not Found", "{path}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_path_is_not_found() {
    let server = start(RoomOptions::default()).await;
    let (status, _) = split(&get(server.local_addr, "/nope").await);
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    server.shutdown().await;
}

#[tokio::test]
async fn a_known_path_with_the_wrong_method_says_so() {
    let server = start(RoomOptions::default()).await;
    let response = request(
        server.local_addr,
        "POST /healthz HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert_eq!(split(&response).0, "HTTP/1.1 405 Method Not Allowed");
    server.shutdown().await;
}

/// A query string is not part of the route.
#[tokio::test]
async fn a_query_string_does_not_defeat_the_router() {
    let server = start(RoomOptions::default()).await;
    let (status, _) = split(&get(server.local_addr, "/healthz?probe=1").await);
    assert_eq!(status, "HTTP/1.1 200 OK");
    server.shutdown().await;
}

#[tokio::test]
async fn a_bad_token_is_refused_and_names_the_scheme() {
    let server = start_with_admin().await;

    let response = authed(server.local_addr, "GET", "/admin/v1/status", "wrong").await;
    assert_eq!(split(&response).0, "HTTP/1.1 401 Unauthorized");
    assert!(response.contains("WWW-Authenticate: Bearer"), "{response}");

    // No header at all is the same answer as the wrong one.
    let (status, _) = split(&get(server.local_addr, "/admin/v1/status").await);
    assert_eq!(status, "HTTP/1.1 401 Unauthorized");

    server.shutdown().await;
}

#[tokio::test]
async fn guessing_the_token_is_rate_limited() {
    let server = start_with_admin().await;
    let mut saw_429 = false;
    // The limit is 10 in a 60s window, so this crosses it comfortably.
    for _ in 0..15 {
        let response = authed(server.local_addr, "GET", "/admin/v1/status", "wrong").await;
        if split(&response).0 == "HTTP/1.1 429 Too Many Requests" {
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "guessing should be cut off");

    // And the cutoff is not an oracle: the right token is refused too, while
    // the window is closed.
    let response = authed(server.local_addr, "GET", "/admin/v1/status", TOKEN).await;
    assert_eq!(split(&response).0, "HTTP/1.1 429 Too Many Requests");

    server.shutdown().await;
}

#[tokio::test]
async fn status_reports_the_room() {
    let server = start_with_admin().await;
    let response = authed(server.local_addr, "GET", "/admin/v1/status", TOKEN).await;
    let (status, body) = split(&response);
    assert_eq!(status, "HTTP/1.1 200 OK");

    let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(json["seed_name"], "56807069331869547085");
    assert_eq!(json["api_version"], 1);

    // A room started without --save-dir keeps nothing, and says so rather than
    // reporting a save that never happens.
    assert!(json["save"].is_null(), "{}", json["save"]);

    let net = &json["net"];
    assert_eq!(net["clients_connected"], 0);
    assert_eq!(net["lag_disconnects"], 0);
    assert!(net["outbound_budget_bytes"].as_u64().unwrap() > 0);

    let slots = json["slots"].as_array().expect("slots");
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0]["connected"], false);
    assert_eq!(slots[0]["checks"], 0);
    assert_eq!(slots[0]["status"], "unknown");

    // The token must never appear in anything the surface renders.
    assert!(!body.contains(TOKEN), "the token was echoed: {body}");

    server.shutdown().await;
}

#[tokio::test]
async fn metrics_are_prometheus_text() {
    let server = start_with_admin().await;
    let response = authed(server.local_addr, "GET", "/admin/v1/metrics", TOKEN).await;
    let (status, body) = split(&response);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        response.contains("Content-Type: text/plain; version=0.0.4"),
        "{response}"
    );

    for expected in [
        "# TYPE pahoa_clients_connected gauge",
        "pahoa_clients_connected 0",
        "# TYPE pahoa_lag_disconnects_total counter",
        "pahoa_slots 2",
        "pahoa_slots_connected 0",
    ] {
        assert!(body.contains(expected), "missing {expected:?} in:\n{body}");
    }

    // Every line is either a comment or `name value`.
    for line in body
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let parts: Vec<&str> = line.split(' ').collect();
        assert_eq!(parts.len(), 2, "malformed exposition line {line:?}");
        assert!(parts[1].parse::<f64>().is_ok(), "not a number: {line:?}");
    }

    server.shutdown().await;
}

/// `POST /admin/v1/shutdown` answers before quiescing, then takes the same exit
/// path SIGTERM does — which the owner of the process observes by awaiting
/// `shutdown_requested`.
#[tokio::test]
async fn shutdown_answers_and_then_asks_the_process_to_stop() {
    let server = Arc::new(start_with_admin().await);

    // Whoever owns the exit is waiting on this, exactly as `serve.rs` does.
    let waiting = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.shutdown_requested().await })
    };

    let response = authed(server.local_addr, "POST", "/admin/v1/shutdown", TOKEN).await;
    let (status, body) = split(&response);
    assert_eq!(status, "HTTP/1.1 202 Accepted");
    assert_eq!(body, "shutting down\n");

    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the shutdown request should have been signalled")
        .expect("the waiter should not have panicked");

    server.shutdown().await;
}

/// A shutdown that nobody is listening for must not wedge the request.
#[tokio::test]
async fn shutdown_answers_even_with_no_one_waiting() {
    let server = start_with_admin().await;
    let response = authed(server.local_addr, "POST", "/admin/v1/shutdown", TOKEN).await;
    assert_eq!(split(&response).0, "HTTP/1.1 202 Accepted");
    server.shutdown().await;
}

/// Shutting down stops taking new connections before the final save runs, so a
/// client cannot arrive during the flush and be told about a room that is
/// already going away.
///
/// The listener going away entirely is why the router's `503` path is only for
/// the narrower race where the actor stops while the listener is still up — a
/// closed port is the better answer, and this is what proves it happens.
#[tokio::test]
async fn the_port_stops_accepting_once_the_room_has_stopped() {
    let server = start_with_admin().await;
    let addr = server.local_addr;
    assert!(
        TcpStream::connect(addr).await.is_ok(),
        "should be accepting while running"
    );

    server.shutdown().await;

    assert!(
        TcpStream::connect(addr).await.is_err(),
        "should have stopped accepting"
    );
}

#[tokio::test]
async fn the_admin_surface_rejects_the_wrong_method() {
    let server = start_with_admin().await;
    let response = authed(server.local_addr, "POST", "/admin/v1/status", TOKEN).await;
    assert_eq!(split(&response).0, "HTTP/1.1 405 Method Not Allowed");

    // And an unknown admin path is still a 404, once authenticated.
    let response = authed(server.local_addr, "GET", "/admin/v1/nope", TOKEN).await;
    assert_eq!(split(&response).0, "HTTP/1.1 404 Not Found");

    server.shutdown().await;
}

/// The whole point of the surface: it shares the port with the game.
#[tokio::test]
async fn the_websocket_still_upgrades_on_the_same_port() {
    let server = start(RoomOptions::default()).await;

    let (mut ws, response) = tokio_tungstenite::client_async(
        format!("ws://{}/", server.local_addr),
        TcpStream::connect(server.local_addr).await.unwrap(),
    )
    .await
    .expect("the upgrade should still be accepted");
    assert_eq!(response.status(), 101);

    // And HTTP still works while it is open.
    let (status, _) = split(&get(server.local_addr, "/healthz").await);
    assert_eq!(status, "HTTP/1.1 200 OK");

    use futures_util::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("RoomInfo arrives")
        .expect("a message")
        .expect("not an error");
    assert!(first.into_text().unwrap().contains("RoomInfo"));

    drop(ws);
    server.shutdown().await;
}
