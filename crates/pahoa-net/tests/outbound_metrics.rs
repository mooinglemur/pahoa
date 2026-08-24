//! The outbound counters, and the distinction they exist to draw.
//!
//! **Its own test binary on purpose.** `pahoa_packets_out_total` is room-wide
//! with no slot label, so it is the one metric a second room in the same
//! process would pollute — and every integration test in `http.rs` is a second
//! room. Separate file, separate process, clean counters.
//!
//! Synthetic multidata rather than a fixture, so these run in CI.

use futures_util::{SinkExt, StreamExt};
use pahoa_multidata::{LocationEntry, LocationStore, MultiData, NetworkSlot, SlotType, Version};
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "test-token-of-at-least-thirty-two-bytes";

fn slot(name: &str, game: &str) -> NetworkSlot {
    NetworkSlot {
        name: name.to_string(),
        game: game.to_string(),
        slot_type: SlotType::Player,
        group_members: Vec::new(),
    }
}

async fn start() -> Server {
    let mut slot_info = BTreeMap::new();
    slot_info.insert(1, slot("Troy", "A Link to the Past"));
    let mut connect_names = HashMap::new();
    connect_names.insert("Troy".to_string(), (0, 1));

    let data = Arc::new(MultiData {
        seed_name: "outbound".to_string(),
        generator_version: Version::new(0, 6, 2),
        minimum_server_version: Version::new(0, 1, 6),
        minimum_client_versions: HashMap::new(),
        slot_info,
        connect_names,
        locations: LocationStore::from_entries(vec![LocationEntry::new(1, 10, 77, 1, 0)], 1),
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
    Server::start(
        Room::new(
            data,
            Arc::new(names),
            RoomOptions::default(),
            1_700_000_000.0,
        ),
        NetConfig {
            port: 0,
            admin_token: Some(TOKEN.to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("binds")
}

struct Ws(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
);

impl Ws {
    async fn open(addr: SocketAddr) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(&format!("ws://{addr}"))
            .await
            .expect("connects");
        let mut client = Self(ws);
        client.wait_for("RoomInfo").await;
        client
    }

    async fn join(addr: SocketAddr) -> Self {
        let mut client = Self::open(addr).await;
        client
            .0
            .send(Message::text(
                json!([{
                    "cmd": "Connect",
                    "password": null,
                    "game": "A Link to the Past",
                    "name": "Troy",
                    "uuid": "outbound-test",
                    "version": {"major": 0, "minor": 9, "build": 0, "class": "Version"},
                    "items_handling": 0b111,
                    "tags": ["AP"],
                    "slot_data": false,
                }])
                .to_string(),
            ))
            .await
            .expect("sends");
        client.wait_for("Connected").await;
        client
    }

    async fn wait_for(&mut self, cmd: &str) -> Value {
        for _ in 0..60 {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.0.next())
                .await
                .expect("no timeout")
                .expect("open")
                .expect("readable");
            let Message::Text(text) = msg else { continue };
            for packet in serde_json::from_str::<Vec<Value>>(&text).expect("JSON") {
                if packet.get("cmd").and_then(Value::as_str) == Some(cmd) {
                    return packet;
                }
            }
        }
        panic!("never saw {cmd}");
    }
}

async fn request(addr: SocketAddr, raw: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connects");
    stream.write_all(raw.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn metrics(addr: SocketAddr) -> String {
    let raw = request(
        addr,
        &format!(
            "GET /admin/v1/metrics HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: 0\r\n\r\n"
        ),
    )
    .await;
    raw.split_once("\r\n\r\n")
        .expect("a complete response")
        .1
        .to_string()
}

async fn admin_say(addr: SocketAddr, text: &str) {
    let body = json!({"command": "say", "text": text}).to_string();
    let response = request(
        addr,
        &format!(
            "POST /admin/v1/command HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "say should run: {response}"
    );
}

/// A metric's value, or 0 when it has no series yet.
fn series(body: &str, prefix: &str) -> u64 {
    body.lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

const TROY: &str = r#"team="0",slot="1",player="Troy",game="A Link to the Past""#;

/// **The whole point of having two counters**, plus where pre-auth traffic
/// lands.
///
/// One broadcast is one thing the room produced and as many deliveries as there
/// are connections to deliver it to. A single per-slot outbound counter would
/// have to be one or the other and would be read as the wrong one.
///
/// **One test rather than two, because the counters are process-wide.** These
/// phases read rows the other phase writes — the pre-auth check asserts no slot
/// has been sent anything, which is only true before anybody joins. As separate
/// `#[test]` functions they would run on parallel threads in this one process
/// and take turns failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_counts_once_and_delivery_counts_per_connection() {
    let server = start().await;
    let addr = server.local_addr;

    // Every connection that opens is sent `RoomInfo` before it holds a slot, so
    // attributing that to a slot is impossible — and dropping it would lose a
    // `DataPackage` answered pre-auth, which can run to megabytes.
    let preauth_before = {
        let _unauth = Ws::open(addr).await;
        let body = metrics(addr).await;
        assert!(
            series(&body, "pahoa_frames_out_preauth_total") >= 1,
            "RoomInfo reached a connection with no slot:\n{body}"
        );
        assert!(
            series(&body, "pahoa_bytes_out_preauth_total") > 0,
            "and it had a size:\n{body}"
        );
        assert!(
            !body.contains(&format!("pahoa_frames_out_total{{{TROY}}}")),
            "nobody has authenticated, so no slot has been sent anything:\n{body}"
        );
        series(&body, "pahoa_frames_out_preauth_total")
    };

    // Two connections on the same slot — co-op, which is ordinary.
    let mut a = Ws::join(addr).await;
    let mut b = Ws::join(addr).await;

    // **A join adds exactly one pre-auth frame, its `RoomInfo`.** The `Connected`
    // reply must land on the slot, and it only does because the transport is
    // told the membership before the reply is dispatched. With those two the
    // other way round it is two frames per join, and `Connected` — which on a
    // large seed carries every slot's info — is filed as anonymous traffic
    // while the slot's own row understates by its biggest packet.
    let joined = metrics(addr).await;
    assert_eq!(
        series(&joined, "pahoa_frames_out_preauth_total") - preauth_before,
        2,
        "two joins, two RoomInfos, and no Connected in the pre-auth bucket:\n{joined}"
    );

    let before = metrics(addr).await;
    let produced_before = series(&before, r#"pahoa_packets_out_total{cmd="PrintJSON"}"#);
    let frames_before = series(&before, &format!("pahoa_frames_out_total{{{TROY}}}"));
    let bytes_before = series(&before, &format!("pahoa_bytes_out_total{{{TROY}}}"));

    // One broadcast, from the admin API so no client packet is involved and
    // nothing else is set in motion.
    admin_say(addr, "exactly one broadcast").await;
    a.wait_for("PrintJSON").await;
    b.wait_for("PrintJSON").await;

    let after = metrics(addr).await;
    let produced = series(&after, r#"pahoa_packets_out_total{cmd="PrintJSON"}"#) - produced_before;
    let frames = series(&after, &format!("pahoa_frames_out_total{{{TROY}}}")) - frames_before;
    let bytes = series(&after, &format!("pahoa_bytes_out_total{{{TROY}}}")) - bytes_before;

    assert_eq!(
        produced, 1,
        "the room emitted one message, whatever its audience:\n{after}"
    );
    assert_eq!(
        frames, 2,
        "both connections of the slot were sent it:\n{after}"
    );
    assert!(bytes > 0, "and the bytes went up with the frames:\n{after}");

    // And the filter counter's `to_slot` half uses the same denominator as the
    // frames it is a share of: per recipient connection, not per slot. The
    // prose said "per recipient ... forty slots is forty", which reads as per
    // slot and is wrong once a slot has a tracker attached.
    // `ServerChat` is what an admin `say` emits; a client `Say` would be `Chat`.
    // Filtering the wrong one drops nothing and proves nothing, which is how
    // this read 0 the first time.
    let put = json!([{"direction": "to_slot", "kind": "print_json", "subtype": "ServerChat"}])
        .to_string();
    let response = request(
        addr,
        &format!(
            "PUT /admin/v1/slots/1/filter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{put}",
            put.len()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

    let dropped_before = series(
        &after,
        &format!(r#"pahoa_filtered_total{{{TROY},direction="to_slot",kind="print_json"}}"#),
    );
    admin_say(addr, "nobody hears this").await;

    // Both connections must be settled, not just one: they may sit on different
    // shards, and only their own shard's mailbox ordering says the broadcast
    // was already handled. A `Sync` reply is queued behind it on that shard, so
    // receiving one means the drop has happened.
    for client in [&mut a, &mut b] {
        client
            .0
            .send(Message::text(json!([{"cmd": "Sync"}]).to_string()))
            .await
            .expect("sends");
        client.wait_for("ReceivedItems").await;
    }

    let muted = metrics(addr).await;
    let dropped = series(
        &muted,
        &format!(r#"pahoa_filtered_total{{{TROY},direction="to_slot",kind="print_json"}}"#),
    ) - dropped_before;
    assert_eq!(
        dropped, 2,
        "one broadcast, two connections on the slot, two drops:\n{muted}"
    );

    server.shutdown().await;
}
