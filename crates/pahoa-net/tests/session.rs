//! End-to-end sessions over real WebSockets.
//!
//! These drive the whole stack — listener, reader task, actor, shards, writer
//! task — with clients that speak the wire protocol exactly as a real
//! Archipelago client does.

use futures_util::{SinkExt, StreamExt};
use pahoa_multidata::{GamePackage, MultiData};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const FIXTURE: &str = "AP_56807069331869547085.archipelago";

fn fixture_dir() -> PathBuf {
    std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crate is two levels below the workspace root")
                .join("crates/pahoa-pickle/tests/fixtures")
        })
}

fn load(name: &str) -> Option<Arc<MultiData>> {
    let raw = std::fs::read(fixture_dir().join(name)).ok()?;
    Some(Arc::new(MultiData::parse(&raw).expect("fixture parses")))
}

async fn start(data: Arc<MultiData>, options: RoomOptions) -> Server {
    let snapshot: BTreeMap<String, GamePackage> = BTreeMap::new();
    let (names, _) = data.resolve_datapackage(&snapshot);
    let room = Room::new(data, Arc::new(names), options, 1_700_000_000.0);
    Server::start(
        room,
        NetConfig {
            port: 0,
            ..Default::default()
        },
    )
    .await
    .expect("server should bind")
}

/// A protocol-level client, speaking exactly what a real one does.
struct Client {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let url = format!("ws://{addr}");
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("should connect");
        Self { ws }
    }

    async fn send(&mut self, packets: Value) {
        self.ws
            .send(Message::text(serde_json::to_string(&packets).unwrap()))
            .await
            .expect("send should succeed");
    }

    /// Next frame, as the array of packets it is.
    async fn recv_frame(&mut self) -> Vec<Value> {
        let msg = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
            .await
            .expect("should not time out")
            .expect("stream should be open")
            .expect("frame should be readable");
        match msg {
            Message::Text(t) => serde_json::from_str(t.as_str()).expect("frame is JSON"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// Read frames until one contains `cmd`, returning that packet.
    async fn wait_for(&mut self, cmd: &str) -> Value {
        for _ in 0..50 {
            for packet in self.recv_frame().await {
                if packet.get("cmd").and_then(Value::as_str) == Some(cmd) {
                    return packet;
                }
            }
        }
        panic!("never saw {cmd}");
    }
}

fn first_player(data: &MultiData) -> (u32, String, String) {
    let (slot, info) = data.player_slots().next().expect("fixture has players");
    (*slot, info.name.clone(), info.game.clone())
}

fn connect_packet(name: &str, game: &str, items_handling: u8) -> Value {
    json!([{
        "cmd": "Connect",
        "password": null,
        "game": game,
        "name": name,
        "uuid": "integration-test",
        "version": {"major": 0, "minor": 6, "build": 8, "class": "Version"},
        "items_handling": items_handling,
        "tags": ["AP"],
        "slot_data": true,
    }])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_connects_and_checks_a_location() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (slot, name, game) = first_player(&data);
    let first_location = data.locations.for_slot(slot)[0].location;
    let server = start(data, RoomOptions::default()).await;

    let mut client = Client::connect(server.local_addr).await;

    // RoomInfo arrives unprompted, before any authentication.
    let room_info = client.wait_for("RoomInfo").await;
    assert!(
        room_info["games"]
            .as_array()
            .unwrap()
            .contains(&json!("Archipelago"))
    );
    assert_eq!(room_info["password"], json!(false));

    client.send(connect_packet(&name, &game, 0b111)).await;
    let connected = client.wait_for("Connected").await;
    assert_eq!(connected["slot"], json!(slot));
    assert!(
        !connected["missing_locations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        connected["checked_locations"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    client
        .send(json!([{"cmd": "LocationChecks", "locations": [first_location]}]))
        .await;

    let update = client.wait_for("RoomUpdate").await;
    assert_eq!(update["checked_locations"], json!([first_location]));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_password_is_refused_over_the_wire() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (_, name, game) = first_player(&data);
    let options = RoomOptions {
        password: Some("hunter2".into()),
        ..Default::default()
    };
    let server = start(data, options).await;

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game, 0b001)).await;

    let refused = client.wait_for("ConnectionRefused").await;
    assert_eq!(refused["errors"], json!(["InvalidPassword"]));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_crosses_between_two_connected_clients() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };

    // A location in one world holding an item for a different slot.
    let mut placement = None;
    for (slot, _) in data.player_slots() {
        if let Some(e) = data
            .locations
            .for_slot(*slot)
            .iter()
            .find(|e| e.receiver != *slot)
        {
            placement = Some((*slot, e.location, e.receiver, e.item));
            break;
        }
    }
    let Some((sender_slot, location, receiver_slot, item_id)) = placement else {
        eprintln!("SKIP: fixture has no cross-slot placement");
        return;
    };
    let info = |s: u32| {
        let i = &data.slot_info[&s];
        (i.name.clone(), i.game.clone())
    };
    let (sn, sg) = info(sender_slot);
    let (rn, rg) = info(receiver_slot);

    let server = start(data, RoomOptions::default()).await;

    let mut sender = Client::connect(server.local_addr).await;
    sender.wait_for("RoomInfo").await;
    sender.send(connect_packet(&sn, &sg, 0b111)).await;
    sender.wait_for("Connected").await;

    let mut receiver = Client::connect(server.local_addr).await;
    receiver.wait_for("RoomInfo").await;
    receiver.send(connect_packet(&rn, &rg, 0b111)).await;
    receiver.wait_for("Connected").await;

    sender
        .send(json!([{"cmd": "LocationChecks", "locations": [location]}]))
        .await;

    let received = receiver.wait_for("ReceivedItems").await;
    let items = received["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["item"], json!(item_id));
    assert_eq!(items[0]["location"], json!(location));
    // `player` is the sending slot everywhere except LocationInfo.
    assert_eq!(items[0]["player"], json!(sender_slot));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_item_feed_reaches_a_bystander() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let players: Vec<u32> = data.player_slots().map(|(s, _)| *s).take(2).collect();
    if players.len() < 2 {
        eprintln!("SKIP: need two player slots");
        return;
    }
    let info = |s: u32| {
        let i = &data.slot_info[&s];
        (i.name.clone(), i.game.clone())
    };
    let (an, ag) = info(players[0]);
    let (bn, bg) = info(players[1]);
    let location = data.locations.for_slot(players[0])[0].location;

    let server = start(data, RoomOptions::default()).await;

    let mut a = Client::connect(server.local_addr).await;
    a.wait_for("RoomInfo").await;
    a.send(connect_packet(&an, &ag, 0b111)).await;
    a.wait_for("Connected").await;

    let mut b = Client::connect(server.local_addr).await;
    b.wait_for("RoomInfo").await;
    b.send(connect_packet(&bn, &bg, 0b111)).await;
    b.wait_for("Connected").await;

    a.send(json!([{"cmd": "LocationChecks", "locations": [location]}]))
        .await;

    // The feed is a broadcast, so an uninvolved player still sees it.
    let printed = b.wait_for("PrintJSON").await;
    assert_eq!(printed["type"], json!("ItemSend"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_frame_drops_the_connection() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let server = start(data, RoomOptions::default()).await;

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;

    // A frame nested past the depth limit. The reference server closes the
    // socket rather than replying, and so do we.
    let deep = "[".repeat(40) + &"]".repeat(40);
    client.ws.send(Message::text(deep)).await.unwrap();

    // The stream should end rather than deliver more packets.
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(msg) = client.ws.next().await {
            match msg {
                Ok(Message::Close(_)) | Err(_) => return true,
                _ => continue,
            }
        }
        true
    })
    .await;
    assert_eq!(ended, Ok(true), "connection should have been closed");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_clients_sustain_a_release_cascade() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };

    // Kept modest by default so the suite stays quick; raise it to push the
    // concurrency model. The point is that broadcast cost is borne by the
    // shards, not the actor, so this should scale close to linearly.
    let count: usize = std::env::var("PAHOA_LOAD_CLIENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    let players: Vec<(u32, String, String)> = data
        .player_slots()
        .map(|(s, i)| (*s, i.name.clone(), i.game.clone()))
        .collect();
    if players.is_empty() {
        eprintln!("SKIP: fixture has no players");
        return;
    }

    let releaser = players[0].clone();
    let all_locations: Vec<i64> = data
        .locations
        .for_slot(releaser.0)
        .iter()
        .map(|e| e.location)
        .collect();
    let server = start(data, RoomOptions::default()).await;

    // Connect the crowd. Slots repeat once the fixture runs out, which is
    // legitimate: co-op means several connections may share a slot.
    let mut clients = Vec::with_capacity(count);
    for i in 0..count {
        let (_, name, game) = &players[i % players.len()];
        let mut c = Client::connect(server.local_addr).await;
        c.wait_for("RoomInfo").await;
        c.send(connect_packet(name, game, 0b001)).await;
        clients.push(c);
    }
    for c in &mut clients {
        c.wait_for("Connected").await;
    }

    let started = std::time::Instant::now();
    let mut releaser_client = Client::connect(server.local_addr).await;
    releaser_client.wait_for("RoomInfo").await;
    releaser_client
        .send(connect_packet(&releaser.1, &releaser.2, 0b111))
        .await;
    releaser_client.wait_for("Connected").await;
    releaser_client
        .send(json!([{"cmd": "LocationChecks", "locations": all_locations}]))
        .await;

    // The releaser's own RoomUpdate confirms the cascade completed.
    let update = releaser_client.wait_for("RoomUpdate").await;
    assert!(!update["checked_locations"].as_array().unwrap().is_empty());
    let elapsed = started.elapsed();

    eprintln!(
        "released {} locations to {count} connected clients in {elapsed:?}",
        all_locations.len()
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "cascade took {elapsed:?}"
    );

    server.shutdown().await;
}

/// A joining client must receive its *own* join announcement.
///
/// The shards filter `AllText` against their own copy of `auth`, and the room
/// published that flag only after the whole handler returned — so the join
/// broadcast went out while the shard still saw the joiner as unauthenticated,
/// and everyone received the message except the client it was about.
///
/// This cannot be caught a level down: `Recorder` resolves recipients against
/// the room itself, where the flag was already set, so the room-level tests all
/// passed. It only exists where two copies of the state do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joining_client_sees_its_own_join_announcement() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (_, name, game) = first_player(&data);
    let server = start(data, RoomOptions::default()).await;

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game, 0b111)).await;

    let mut join_text = None;
    'scan: for _ in 0..50 {
        for packet in client.recv_frame().await {
            match packet["cmd"].as_str() {
                Some("PrintJSON") if packet["type"] == json!("Join") => {
                    join_text = Some(
                        packet["data"]
                            .as_array()
                            .expect("PrintJSON carries data")
                            .iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<String>(),
                    );
                }
                // Sent after the announcement, so its arrival bounds the scan.
                Some("Connected") => break 'scan,
                _ => {}
            }
        }
    }

    let text = join_text.expect("the joining client should receive its own Join");
    assert!(text.contains(&name), "{text}");
    assert!(text.contains("has joined."), "{text}");

    server.shutdown().await;
}
