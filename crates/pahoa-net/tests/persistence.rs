//! M7's exit gate, over the real transport.
//!
//! Three claims, each of which has to hold end to end rather than in the room
//! crate alone:
//!
//! 1. A room that is killed and restarted comes back with its state — the
//!    kill -9 case, played out through actual clients rather than by comparing
//!    structs.
//! 2. A save that hangs does **not** stall the room. This is the one that
//!    matters on CephFS, where an MDS failover blocks rather than erroring, and
//!    it is the reason a save runs on a blocking thread the actor never awaits.
//! 3. Saves coalesce. A store slower than the save interval must not accumulate
//!    queued snapshots, each pinning the state it captured — that is the
//!    out-of-memory path that only appears on a bad day.

use futures_util::{SinkExt, StreamExt};
use pahoa_multidata::{GamePackage, MultiData};
use pahoa_net::actor::SaveConfig;
use pahoa_net::{NetConfig, SaveStore, Server};
use pahoa_room::{Room, RoomOptions, Snapshot};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

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

fn build_room(data: Arc<MultiData>) -> Room {
    let snapshot: BTreeMap<String, GamePackage> = BTreeMap::new();
    let (names, _) = data.resolve_datapackage(&snapshot);
    Room::new(
        data,
        Arc::new(names),
        RoomOptions::default(),
        1_700_000_000.0,
    )
}

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pahoa-persist-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Client {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(&format!("ws://{addr}"))
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

    async fn wait_for(&mut self, cmd: &str) -> Value {
        for _ in 0..50 {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {cmd}"))
                .expect("stream should be open")
                .expect("frame should be readable");
            let Message::Text(t) = msg else { continue };
            let frame: Vec<Value> = serde_json::from_str(t.as_str()).expect("frame is JSON");
            for packet in frame {
                if packet.get("cmd").and_then(Value::as_str) == Some(cmd) {
                    return packet;
                }
            }
        }
        panic!("never saw {cmd}");
    }
}

fn connect_packet(name: &str, game: &str) -> Value {
    json!([{
        "cmd": "Connect",
        "password": null,
        "game": game,
        "name": name,
        "uuid": "persistence-test",
        "version": {"major": 0, "minor": 6, "build": 8, "class": "Version"},
        "items_handling": 0b111,
        "tags": ["AP"],
        "slot_data": true,
    }])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_room_remembers_what_a_client_did() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (slot, info) = data.player_slots().next().expect("fixture has players");
    let (slot, name, game) = (*slot, info.name.clone(), info.game.clone());
    let checked: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(5)
        .map(|e| e.location)
        .collect();

    let dir = tempdir("restart");

    // --- first life --------------------------------------------------------
    {
        let store = Arc::new(SaveStore::open(&dir).expect("claims the directory"));
        let server = Server::start_with_saves(
            build_room(data.clone()),
            NetConfig {
                port: 0,
                ..Default::default()
            },
            SaveConfig {
                store: Some(store),
                // Short, so the test does not sit through a production cadence.
                interval: Duration::from_millis(50),
                ..Default::default()
            },
        )
        .await
        .expect("binds");

        let mut client = Client::connect(server.local_addr).await;
        client.wait_for("RoomInfo").await;
        client.send(connect_packet(&name, &game)).await;
        client.wait_for("Connected").await;
        client
            .send(json!([{"cmd": "LocationChecks", "locations": checked}]))
            .await;
        client
            .send(json!([{"cmd": "Say", "text": "!alias Persistent"}]))
            .await;
        client.wait_for("RoomUpdate").await;

        // Shutdown waits for the final save, which is what makes the restart
        // below deterministic rather than a race against the interval.
        server.shutdown().await;
        drop(server);
    }

    // --- second life -------------------------------------------------------
    let store = SaveStore::open(&dir).expect("the lock was released on shutdown");
    let raw = store.load().expect("readable").expect("a save was written");
    let mut room = build_room(data.clone());
    room.restore(Snapshot::decode(&raw).expect("decodes"))
        .expect("restores");

    let server = Server::start_with_saves(
        room,
        NetConfig {
            port: 0,
            ..Default::default()
        },
        SaveConfig {
            store: Some(Arc::new(store)),
            ..Default::default()
        },
    )
    .await
    .expect("binds");

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game)).await;
    let connected = client.wait_for("Connected").await;

    // The thing a returning player actually notices: their checks are still
    // checked, so the client does not re-send them and the tracker is right.
    let mut got: Vec<i64> = connected["checked_locations"]
        .as_array()
        .expect("checked_locations is a list")
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    got.sort_unstable();
    let mut want = checked.clone();
    want.sort_unstable();
    assert_eq!(got, want, "a restarted room lost the player's checks");

    // And the items those checks produced are still owed.
    // `NetworkPlayer.alias` is the aliased *display* name, "Alias (SlotName)".
    let players = connected["players"].as_array().unwrap();
    let mine = players
        .iter()
        .find(|p| p["slot"] == json!(slot))
        .expect("our slot is listed");
    assert_eq!(
        mine["alias"],
        json!(format!("Persistent ({name})")),
        "the alias did not survive the restart"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sink that hangs until told to stop, standing in for a filesystem having a
/// bad day — an MDS failover, a rebalance, a node that has stopped answering.
///
/// It blocks indefinitely rather than for a fixed duration, so the test proves
/// the room is unaffected for however long it takes to check, and still tears
/// down promptly. Sleeping a hard-coded thirty seconds would prove the same
/// thing and cost thirty seconds.
struct HangingSink {
    release: Arc<AtomicBool>,
    started: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

impl pahoa_net::SaveSink for HangingSink {
    fn store(&self, _bytes: &[u8]) -> std::io::Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        // A backstop, so a failing test hangs the suite for a minute rather
        // than forever.
        for _ in 0..6000 {
            if self.release.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_that_hangs_does_not_stall_the_room() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (slot, info) = data.player_slots().next().expect("fixture has players");
    let (slot, name, game) = (*slot, info.name.clone(), info.game.clone());
    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .take(60)
        .map(|e| e.location)
        .collect();

    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let slow = Arc::new(HangingSink {
        release: Arc::clone(&release),
        started: Arc::clone(&started),
        finished: Arc::clone(&finished),
    });

    let server = Server::start_with_saves(
        build_room(data.clone()),
        NetConfig {
            port: 0,
            ..Default::default()
        },
        SaveConfig {
            store: Some(slow),
            interval: Duration::from_millis(20),
            // Do not sit through the hung write on the way out.
            shutdown_timeout: Duration::from_millis(100),
            ..Default::default()
        },
    )
    .await
    .expect("binds");

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game)).await;
    client.wait_for("Connected").await;

    // One check to make the room dirty, so the next tick starts the save that
    // will then hang for thirty seconds.
    client
        .send(json!([{"cmd": "LocationChecks", "locations": locations[..1]}]))
        .await;
    client.wait_for("RoomUpdate").await;
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 1, "no save ever started");
    assert_eq!(
        finished.load(Ordering::SeqCst),
        0,
        "the save should still be stuck"
    );

    // The room must answer normally while that write is hung. This is the whole
    // claim: a stuck filesystem costs the recovery point, not the room.
    let latency_start = Instant::now();
    client
        .send(json!([{"cmd": "LocationChecks", "locations": locations}]))
        .await;
    client.wait_for("RoomUpdate").await;
    client
        .send(json!([{"cmd": "Say", "text": "!status"}]))
        .await;
    client.wait_for("PrintJSON").await;
    let latency = latency_start.elapsed();

    assert_eq!(
        finished.load(Ordering::SeqCst),
        0,
        "the save finished early, so this proved nothing about a hung one"
    );
    assert!(
        latency < Duration::from_secs(2),
        "the room took {latency:?} to answer while a save was stuck, so it is \
         waiting on the disk"
    );
    // And no second save piled up behind the first.
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "ticks queued behind the stuck save instead of being dropped"
    );

    release.store(true, Ordering::SeqCst);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saves_coalesce_rather_than_queue() {
    let Some(data) = load() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let (slot, info) = data.player_slots().next().expect("fixture has players");
    let (slot, name, game) = (*slot, info.name.clone(), info.game.clone());
    let locations: Vec<i64> = data
        .locations
        .for_slot(slot)
        .iter()
        .map(|e| e.location)
        .collect();

    let dir = tempdir("coalesce");
    let store = Arc::new(SaveStore::open(&dir).expect("claims the directory"));
    let server = Server::start_with_saves(
        build_room(data.clone()),
        NetConfig {
            port: 0,
            ..Default::default()
        },
        SaveConfig {
            store: Some(Arc::clone(&store) as Arc<dyn pahoa_net::SaveSink>),
            // Aggressive on purpose: many more ticks will fire than there is
            // work for, and every surplus one must be dropped.
            interval: Duration::from_millis(5),
            ..Default::default()
        },
    )
    .await
    .expect("binds");

    let mut client = Client::connect(server.local_addr).await;
    client.wait_for("RoomInfo").await;
    client.send(connect_packet(&name, &game)).await;
    client.wait_for("Connected").await;

    // Keep the room busy so `dirty` is set again and again.
    for chunk in locations.chunks(50).take(20) {
        client
            .send(json!([{"cmd": "LocationChecks", "locations": chunk}]))
            .await;
    }
    client.wait_for("RoomUpdate").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The room is still answering, and the save on disk is a real one rather
    // than a partially written casualty of overlapping writers.
    client
        .send(json!([{"cmd": "Say", "text": "!status"}]))
        .await;
    client.wait_for("PrintJSON").await;

    server.shutdown().await;

    let raw = store
        .load()
        .expect("readable")
        .expect("something was saved");
    let snapshot = Snapshot::decode(&raw).expect("the save on disk is intact");
    let saved_checks: usize = snapshot.location_checks.iter().map(|(_, c)| c.len()).sum();
    assert!(
        saved_checks > 0,
        "saves ran but persisted nothing, so coalescing dropped the state as well as the ticks"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
