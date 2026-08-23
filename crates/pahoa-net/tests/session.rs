//! End-to-end sessions over real WebSockets.
//!
//! These drive the whole stack — listener, reader task, actor, shards, writer
//! task — with clients that speak the wire protocol exactly as a real
//! Archipelago client does.

use futures_util::{SinkExt, StreamExt};
use pahoa_multidata::MultiData;
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

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
    start_filtered(data, options, None).await
}

/// The same, with a room-wide send filter already in place.
///
/// Set before the server starts because the room pushes each connection's
/// filter to the transport as it authenticates, so a filter configured up front
/// is live for everyone who joins.
async fn start_filtered(
    data: Arc<MultiData>,
    options: RoomOptions,
    filter: Option<serde_json::Value>,
) -> Server {
    let (names, _) = data.resolve_datapackage();
    let mut room = Room::new(data, Arc::new(names), options, 1_700_000_000.0);
    if let Some(rules) = filter {
        let mut sink = pahoa_room::Recorder::default();
        room.set_filter(
            None,
            Some(pahoa_room::filter::Filter::from_json(&rules).expect("valid rules")),
            &mut sink,
        );
    }
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
                let seen = packet.get("cmd").and_then(Value::as_str);
                if seen == Some(cmd) {
                    return packet;
                }
                // A refused connection never sends anything else, so waiting on
                // it is a hang rather than a failure. Say what the room said
                // instead: this is how a fixture whose slots demand a newer
                // client than the tests claim announces itself.
                if seen == Some("ConnectionRefused") && cmd != "ConnectionRefused" {
                    panic!(
                        "waiting for {cmd}, but the room refused the connection: {}",
                        packet.get("errors").unwrap_or(&Value::Null)
                    );
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

/// The client version these tests claim.
///
/// Deliberately ahead of any fixture's floor rather than matching a real
/// client: a seed carries a *per-slot* minimum client version, and a slot that
/// demands more than this is refused — correctly — which leaves a test waiting
/// for a `Connected` that will never arrive. The fixture that prompted this
/// number has a slot requiring 0.7.0.
///
/// If a new fixture refuses a connection for `IncompatibleVersion`, raise this;
/// nothing here is testing version negotiation.
const CLIENT_VERSION: (u32, u32, u32) = (0, 9, 0);

fn connect_packet(name: &str, game: &str, items_handling: u8) -> Value {
    json!([{
        "cmd": "Connect",
        "password": null,
        "game": game,
        "name": name,
        "uuid": "integration-test",
        "version": {
            "major": CLIENT_VERSION.0,
            "minor": CLIENT_VERSION.1,
            "build": CLIENT_VERSION.2,
            "class": "Version",
        },
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

    // The feed is a broadcast, so an uninvolved player still sees it. `b`'s own
    // join and tutorial lines now arrive *after* its `Connected` — the reply to
    // `Connect` comes first — so scan for the one this test is about rather
    // than taking whichever PrintJSON lands next.
    let mut printed = b.wait_for("PrintJSON").await;
    for _ in 0..10 {
        if printed["type"] == json!("ItemSend") {
            break;
        }
        printed = b.wait_for("PrintJSON").await;
    }
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

/// A joining client must receive its *own* join announcement — **after** the
/// `Connected` that answers its `Connect`.
///
/// Two bugs meet here, so the test asserts both.
///
/// The shards filter `AllText` against their own copy of `auth`, and the room
/// published that flag only after the whole handler returned — so the join
/// broadcast went out while the shard still saw the joiner as unauthenticated,
/// and everyone received the message except the client it was about. That
/// cannot be caught a level down: `Recorder` resolves recipients against the
/// room itself, where the flag was already set, so the room-level tests all
/// passed. It only exists where two copies of the state do.
///
/// The order is the second. `Connected` is the reply to `Connect` and has to
/// arrive first, even though `MultiServer.py:1936-1939` announces the join
/// *before* sending it — the reference's announcements are `async_start`
/// tasks that cannot run until the handler yields, so its wire order is the
/// reverse of its source order. A client written against the reference is
/// entitled to `Connected` first, and one of them refuses the connection
/// outright when a `PrintJSON` arrives where `Connected` was expected.
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
    let mut seen_connected = false;
    'scan: for _ in 0..50 {
        for packet in client.recv_frame().await {
            match packet["cmd"].as_str() {
                Some("Connected") => seen_connected = true,
                Some("PrintJSON") if packet["type"] == json!("Join") => {
                    assert!(
                        seen_connected,
                        "the join announcement overtook the Connected that answers Connect; \
                         a client that requires the reply first will drop the connection"
                    );
                    join_text = Some(
                        packet["data"]
                            .as_array()
                            .expect("PrintJSON carries data")
                            .iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<String>(),
                    );
                    break 'scan;
                }
                _ => {}
            }
        }
    }

    assert!(seen_connected, "no Connected arrived at all");
    let text = join_text.expect("the joining client should receive its own Join");
    assert!(text.contains(&name), "{text}");
    assert!(text.contains("has joined."), "{text}");

    server.shutdown().await;
}

/// **`Connected` is the first thing a client hears after `Connect`.**
///
/// A client is entitled to treat the reply to `Connect` as the next packet it
/// receives — the protocol documents `Connected` as that reply — and at least
/// one refuses the connection outright with
/// `IllegalResponse { expected: "Connected", received: "PrintJSON" }` when
/// anything else arrives first.
///
/// pahoa got this wrong by copying the reference's *source* order, where
/// `on_client_joined` runs before `send_msgs`. That is not the reference's
/// *wire* order: its announcements go out through `async_start`, so they are
/// tasks that cannot run until the handler yields, and the awaited send reaches
/// the socket first. Ordering here has to be asserted against the wire, which
/// is why this test lives at the session level and not against `Recorder`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_precedes_every_other_packet() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (_, name, game) = first_player(&data);
    let server = start(data, RoomOptions::default()).await;

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game, 0b111)).await;

    // The very first packet of the very first frame, whatever it is.
    let mut first = None;
    for _ in 0..50 {
        if let Some(packet) = client.recv_frame().await.into_iter().next() {
            first = packet["cmd"].as_str().map(str::to_string);
            break;
        }
    }

    assert_eq!(
        first.as_deref(),
        Some("Connected"),
        "the first packet after Connect must be its reply, not a broadcast that \
         happened to be queued first"
    );

    server.shutdown().await;
}

/// **The send half, over a real socket** — the only place it can be tested.
///
/// A slot's *receive* filter runs in the room and a `Recorder` sees it, but the
/// send half runs in the shard, where a broadcast's audience is expanded. The
/// room never learns who a broadcast reached, so nothing below this level can
/// tell whether a recipient was skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_filter_keeps_one_print_type_away_from_a_client() {
    let Some(data) = load(FIXTURE) else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (_, name, game) = first_player(&data);
    let server = start_filtered(
        data,
        RoomOptions::default(),
        Some(json!([{"direction": "to_slot", "kind": "print_json", "subtype": "Chat"}])),
    )
    .await;

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game, 0b111)).await;
    client.wait_for("Connected").await;

    // Say something. The room broadcasts it as a Chat, which this slot filters.
    client.send(json!([{"cmd": "Say", "text": "hello"}])).await;
    // And run a command, whose reply is a different print type — the positive
    // control, so a test that filtered *everything* could not pass.
    client
        .send(json!([{"cmd": "Say", "text": "!players"}]))
        .await;

    // The control has to be **ordered after** the chat, or the scan can finish
    // before the chat would have arrived and pass without proving anything —
    // which is exactly what an earlier version of this test did, happily
    // filtering `Hint` and still reporting no chat. `!players` is a `Say`, so
    // the room broadcasts its echo as Chat *before* replying with a
    // CommandResult; seeing the reply therefore means every chat this test
    // could produce has already been delivered or dropped.
    let mut saw_chat = false;
    let mut saw_control = false;
    for _ in 0..30 {
        for packet in client.recv_frame().await {
            if packet["cmd"] != json!("PrintJSON") {
                continue;
            }
            match packet["type"].as_str() {
                Some("Chat") => saw_chat = true,
                Some("CommandResult") => saw_control = true,
                _ => {}
            }
        }
        if saw_control {
            break;
        }
    }

    assert!(
        saw_control,
        "the control never arrived, so this test proves nothing"
    );
    assert!(
        !saw_chat,
        "a filtered print type reached the client it was filtered for"
    );

    server.shutdown().await;
}
