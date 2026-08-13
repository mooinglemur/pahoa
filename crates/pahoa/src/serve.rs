//! `pahoa serve` — host a multiworld.

use pahoa_multidata::{DataPackage, GamePackage, MultiData};
use pahoa_net::actor::SaveConfig;
use pahoa_net::{NetConfig, SaveStore, Server, build_runtime};
use pahoa_room::{Room, RoomOptions, Snapshot};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct ServeArgs<'a> {
    pub multidata: &'a Path,
    pub snapshot: Option<&'a Path>,
    pub port: u16,
    pub bind: String,
    pub password: Option<String>,
    /// Where the room persists itself. `None` runs without saving at all, which
    /// is fine for a throwaway room and a data-loss bug for anything else.
    pub save_dir: Option<&'a Path>,
    pub save_interval: Duration,
}

pub fn run(args: ServeArgs<'_>) -> Result<(), String> {
    let raw =
        std::fs::read(args.multidata).map_err(|e| format!("{}: {e}", args.multidata.display()))?;
    let data =
        Arc::new(MultiData::parse(&raw).map_err(|e| format!("{}: {e}", args.multidata.display()))?);

    let snapshot: BTreeMap<String, GamePackage> = match args.snapshot {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            DataPackage::load_snapshot(&text).map_err(|e| format!("{}: {e}", p.display()))?
        }
        None => BTreeMap::new(),
    };

    let (names, report) = data.resolve_datapackage(&snapshot);
    if !report.unresolved.is_empty() {
        // Not fatal — names degrade to "Unknown item (ID:n)" — but an operator
        // should be told rather than left to notice in chat.
        eprintln!(
            "warning: no data package for {} game(s): {}",
            report.unresolved.len(),
            report.unresolved.join(", ")
        );
    }
    if !report.missing_hint_blacklist.is_empty() {
        eprintln!(
            "warning: no hint blacklist for {} game(s); !hint will not refuse \
             non-hintable names for them (export one with tools/export-datapackage.py)",
            report.missing_hint_blacklist.len()
        );
    }

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let options = RoomOptions {
        password: args.password,
        ..Default::default()
    };
    let mut room = Room::new(data.clone(), Arc::new(names), options, start_time);

    // Claim the save directory and restore *before* binding, so a client can
    // never reach a half-loaded room, and so a directory another pod already
    // holds fails here rather than after we have started answering.
    let saves = match args.save_dir {
        None => {
            eprintln!("warning: no --save-dir, so this room keeps nothing across a restart");
            SaveConfig {
                store: None,
                ..Default::default()
            }
        }
        Some(dir) => {
            let store = SaveStore::open(dir)
                .map_err(|e| format!("save directory {}: {e}", dir.display()))?;
            match load_save(&store, dir)? {
                Some(snapshot) => {
                    let slots = snapshot.location_checks.len();
                    room.restore(snapshot)
                        .map_err(|e| format!("{}: {e}", store.path().display()))?;
                    println!("restored {} from {}", plural(slots), store.path().display());
                }
                None => println!("no save in {}; starting fresh", dir.display()),
            }
            SaveConfig {
                store: Some(Arc::new(store)),
                interval: args.save_interval,
                ..Default::default()
            }
        }
    };

    let config = NetConfig {
        bind: args.bind,
        port: args.port,
        ..Default::default()
    };
    let runtime = build_runtime(&config).map_err(|e| format!("runtime: {e}"))?;

    runtime.block_on(async move {
        let server = Server::start_with_saves(room, config, saves)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        println!(
            "pahoa serving {} slots, {} locations, seed {} on {}",
            data.slot_info.len(),
            data.locations.len(),
            data.seed_name,
            server.local_addr,
        );

        tokio::signal::ctrl_c().await.ok();
        println!("shutting down");
        server.shutdown().await;
        Ok(())
    })
}

/// Read and decode the save, complaining if the filesystem goes quiet.
///
/// A CephFS MDS failover **blocks** rather than erroring, and a blocked read is
/// uninterruptible — no timeout in userspace can cut it short. What a watchdog
/// can do is say so: without it, Kubernetes sees only a pod that never becomes
/// ready and gives an operator nothing to go on.
fn load_save(store: &SaveStore, dir: &Path) -> Result<Option<Snapshot>, String> {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let done = Arc::clone(&done);
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(500));
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }
            eprintln!(
                "warning: reading the save in {} is taking a long time. If this is a \
                 network filesystem it may be recovering; the room will start once it \
                 responds.",
                dir.display()
            );
        })
    };

    let raw = store
        .load()
        .map_err(|e| format!("{}: {e}", store.path().display()));
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog.join();

    match raw? {
        None => Ok(None),
        Some(bytes) => Snapshot::decode(&bytes)
            .map(Some)
            .map_err(|e| format!("{}: {e}", store.path().display())),
    }
}

fn plural(slots: usize) -> String {
    match slots {
        1 => "1 slot".to_string(),
        n => format!("{n} slots"),
    }
}
