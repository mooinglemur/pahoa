//! M9's load track: 6000 connections against a live room.
//!
//! Differential testing proves *fidelity* at small scale and can say nothing
//! about *scale* — the Python server cannot host this at all, which is most of
//! why pahoa exists. So this is a separate track with its own instrument.
//!
//! It drives an in-process [`pahoa_net::Server`] rather than a socket to another
//! binary, so the metrics the plan asks for — actor mailbox depth, outbound
//! bytes against the global budget, lag disconnects, compressions — are readable
//! directly instead of inferred.
//!
//! ```sh
//! ulimit -n 65536
//! cargo run --release -p pahoa-net --example loadtest -- \
//!     crates/pahoa-pickle/tests/fixtures/SYNTH_2000slot.archipelago 6000
//! ```
//!
//! Three phases, in the order the plan names them:
//!
//! 1. **Connect storm** — all N connections join at once, each demanding
//!    `Connected` with its full `checked_locations` and item queue.
//! 2. **Steady mix** — check traffic, chat and datastorage churn together.
//! 3. **Mass release cascade** — every slot releases, which is the worst case
//!    the whole fan-out design exists for.
//! 4. **Reconnect storm** — every connection drops and rejoins at once, each
//!    demanding a full resync. This is the phase that most resembles a restart
//!    in production.

use pahoa_multidata::MultiData;
use pahoa_net::ws::client::Client;
use pahoa_net::{NetConfig, Server};
use pahoa_room::{Room, RoomOptions};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static CONNECTED: AtomicUsize = AtomicUsize::new(0);
static MESSAGES: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);

type Fallible = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Fallible> {
    tracing_subscriber_init();

    let mut args = std::env::args().skip(1);
    let fixture = args.next().unwrap_or_else(|| {
        "crates/pahoa-pickle/tests/fixtures/SYNTH_2000slot.archipelago".to_string()
    });
    let target: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(6000);

    let raw = std::fs::read(Path::new(&fixture))?;
    let data = Arc::new(MultiData::parse(&raw)?);
    let slots: Vec<(u32, String, String)> = data
        .player_slots()
        .map(|(s, i)| (*s, i.name.clone(), i.game.clone()))
        .collect();
    println!(
        "fixture: {} slots, {} locations, seed {}",
        slots.len(),
        data.locations.len(),
        data.seed_name
    );
    if slots.is_empty() {
        return Err("fixture has no player slots".into());
    }

    let (names, _) = data.resolve_datapackage();
    // `Enabled` so every slot can release its own world on request, which is
    // how the cascade below is driven. The default `Auto` would refuse.
    let room = Room::new(
        data.clone(),
        Arc::new(names),
        RoomOptions {
            release_mode: pahoa_proto::Permission::Enabled,
            ..Default::default()
        },
        0.0,
    );
    let config = NetConfig {
        port: 0,
        ..Default::default()
    };
    println!(
        "runtime: {} worker threads, {} shards",
        config.worker_threads_resolved(),
        config.shards_resolved()
    );
    let server = Server::start(room, config).await?;
    let addr = server.local_addr;
    println!("serving on {addr}\n");

    // --- phase 1: connect storm ------------------------------------------
    //
    // Players commonly run a game client plus a text client plus a tracker, so
    // the connection count deliberately exceeds the slot count — several
    // connections share a slot, which is also what exercises the co-op path.
    let mark = Mark::now();
    let mut handles = Vec::with_capacity(target);
    for i in 0..target {
        let (_, name, game) = slots[i % slots.len()].clone();
        handles.push(tokio::spawn(async move {
            let client = match connect_and_join(addr, &name, &game).await {
                Ok(client) => client,
                Err(e) => {
                    if FAILURES.fetch_add(1, Ordering::Relaxed) < 5 {
                        eprintln!("connect failed: {e}");
                    }
                    return None;
                }
            };
            CONNECTED.fetch_add(1, Ordering::Relaxed);

            // Start reading *immediately*, before the other connections have
            // finished joining. A real client does, and it matters enormously
            // here: every join is announced to everyone, so a client that waits
            // until the storm is over accumulates one announcement per other
            // connection. At this scale that is gigabytes of queued spam, and
            // the server would rightly drop connections that are not actually
            // slow — the harness would have manufactured its own failure.
            let (mut reader, mut writer) = client.into_split();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let read_task = tokio::spawn(async move {
                // Frames are counted and discarded rather than inflated: the
                // question is what the *server* costs, and a client that
                // inflates every broadcast is more expensive than the server
                // that compressed it once. The server still does all its own
                // work — the extension is negotiated, the payload compressed,
                // the bytes written.
                if let Ok(n) = reader.discard().await {
                    MESSAGES.fetch_add(n, Ordering::Relaxed);
                }
            });
            let write_task = tokio::spawn(async move {
                while let Some(text) = rx.recv().await {
                    if writer.send(&text).await.is_err() {
                        return;
                    }
                }
            });
            Some((tx, read_task, write_task))
        }));
    }

    let mut senders = Vec::with_capacity(target);
    let mut readers = Vec::with_capacity(target * 2);
    for handle in handles {
        if let Ok(Some((tx, read_task, write_task))) = handle.await {
            senders.push(tx);
            readers.push(read_task);
            readers.push(write_task);
        }
    }
    settle(Duration::from_secs(30)).await;
    report(
        "connect storm",
        mark,
        &format!("{} of {target} joined", senders.len()),
    );
    if senders.is_empty() {
        return Err("no connections survived; check `ulimit -n`".into());
    }

    // --- phase 2: steady mix ---------------------------------------------
    // Chat and datastorage churn from many connections at once, which is the
    // ordinary traffic the room has to keep serving while everything else
    // happens.
    let mark = Mark::now();
    let chatters = senders.len().min(200);
    for round in 0..20 {
        for (i, tx) in senders.iter().take(chatters).enumerate() {
            let _ = tx.send(format!(
                r#"[{{"cmd":"Say","text":"round {round} from {i}"}}]"#
            ));
            let _ = tx.send(format!(
                r#"[{{"cmd":"Set","key":"tracker_{i}","operations":[{{"operation":"replace","value":{round}}}]}}]"#
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    settle(Duration::from_secs(2)).await;
    report(
        "steady mix",
        mark,
        &format!("{} chat + {} datastorage", chatters * 20, chatters * 20),
    );

    // --- phase 3: mass release cascade -----------------------------------
    //
    // Every slot gives up its remaining items at once. This is the worst case
    // the whole fan-out design exists for: at 2000 slots it is hundreds of
    // thousands of item events, each fanned out to every connection.
    let mark = Mark::now();
    for tx in &senders {
        // Each connection releases *its own* world — sending another slot's
        // location ids would simply be dropped as unknown, which is what makes
        // `!release` the right lever here rather than `LocationChecks`.
        let _ = tx.send(r#"[{"cmd":"Say","text":"!release"}]"#.to_string());
    }
    settle(Duration::from_secs(30)).await;
    report("mass release", mark, "every slot released its world");

    // --- phase 4: reconnect storm ----------------------------------------
    //
    // Every connection goes away and comes back at once, each demanding a full
    // `Connected` resync — the shape of a server restart, and the phase where
    // the room is doing the most per-connection work it ever does.
    drop(senders);
    for reader in &readers {
        reader.abort();
    }
    // Let the server notice the disconnects, so this measures reconnection
    // rather than reconnection tangled with 6000 teardowns.
    settle(Duration::from_secs(5)).await;
    let mark = Mark::now();
    let mut rejoined = Vec::new();
    for i in 0..target {
        let (_, name, game) = slots[i % slots.len()].clone();
        rejoined.push(tokio::spawn(async move {
            connect_and_join(addr, &name, &game).await.is_ok()
        }));
    }
    let mut ok = 0;
    for handle in rejoined {
        if handle.await.unwrap_or(false) {
            ok += 1;
        }
    }
    report(
        "reconnect storm",
        mark,
        &format!(
            "{ok} of {target} resynced after a full release \
             (its lag disconnects are the old connections, aborted on purpose above)"
        ),
    );

    println!("\nfinal: {}", pahoa_net::metrics::summary());
    server.shutdown().await;
    Ok(())
}

/// Connect, authenticate, and wait for the server to finish the handshake.
///
/// Waiting for `Connected` matters: it is the packet carrying the full
/// `checked_locations` resync, so a reconnect storm is only measured properly
/// once every client has actually received one.
async fn connect_and_join(
    addr: std::net::SocketAddr,
    name: &str,
    game: &str,
) -> Result<Client, Fallible> {
    let mut client = Client::connect(addr, true).await?;
    client.wait_for("RoomInfo").await?;
    client
        .send(&format!(
            r#"[{{"cmd":"Connect","password":null,"game":"{game}","name":"{name}",
               "uuid":"load","version":{{"major":0,"minor":6,"build":8,"class":"Version"}},
               "items_handling":7,"tags":["AP"],"slot_data":false}}]"#
        ))
        .await?;
    match client.wait_for("Connected").await? {
        Some(_) => Ok(client),
        None => Err("no Connected before the socket closed".into()),
    }
}

/// Wait for the room to go quiet, or give up after `limit`.
///
/// Quiet means the outbound budget is empty and the actor's mailbox has drained
/// — measuring the moment work was *queued* rather than finished would flatter
/// every number here.
async fn settle(limit: Duration) {
    let deadline = Instant::now() + limit;
    let mut quiet_rounds = 0;
    while Instant::now() < deadline {
        if pahoa_net::budget::queued_bytes() == 0 && pahoa_net::metrics::mailbox_depth() == 0 {
            quiet_rounds += 1;
            if quiet_rounds >= 3 {
                return;
            }
        } else {
            quiet_rounds = 0;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Counters at the start of a phase, so what a phase *caused* is separable from
/// what happened before it. Cumulative totals hide exactly the thing worth
/// knowing — which phase produced the lag disconnects.
#[derive(Clone, Copy)]
struct Mark {
    at: Instant,
    compressions: u64,
    lag: u64,
    messages: u64,
}

impl Mark {
    fn now() -> Self {
        Self {
            at: Instant::now(),
            compressions: pahoa_net::ws::deflate::compressions(),
            lag: pahoa_net::metrics::lag_disconnects(),
            messages: MESSAGES.load(Ordering::Relaxed),
        }
    }
}

fn report(phase: &str, mark: Mark, detail: &str) {
    let elapsed = mark.at.elapsed();
    println!(
        "{phase:<16} {elapsed:>9.2?}  {detail}\n\
         {:<16} +{} compressions, +{} messages, +{} lag disconnects\n\
         {:<16} mailbox {} (peak {}), outbound peak {} KiB, rss {}",
        "",
        pahoa_net::ws::deflate::compressions() - mark.compressions,
        MESSAGES.load(Ordering::Relaxed) - mark.messages,
        pahoa_net::metrics::lag_disconnects() - mark.lag,
        "",
        pahoa_net::metrics::mailbox_depth(),
        pahoa_net::metrics::mailbox_peak(),
        pahoa_net::budget::peak_bytes() >> 10,
        pahoa_net::metrics::resident_bytes().map_or("?".into(), |b| format!("{} MiB", b >> 20)),
    );
}

fn tracing_subscriber_init() {
    // Nothing configured: the run reports through the metrics above, and log
    // lines at this connection count would be the load rather than a view of it.
}
