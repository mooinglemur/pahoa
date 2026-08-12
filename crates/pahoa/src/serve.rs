//! `pahoa serve` — host a multiworld.

use pahoa_multidata::{DataPackage, GamePackage, MultiData};
use pahoa_net::{NetConfig, Server, build_runtime};
use pahoa_room::{Room, RoomOptions};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ServeArgs<'a> {
    pub multidata: &'a Path,
    pub snapshot: Option<&'a Path>,
    pub port: u16,
    pub bind: String,
    pub password: Option<String>,
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
    let room = Room::new(data.clone(), Arc::new(names), options, start_time);

    let config = NetConfig {
        bind: args.bind,
        port: args.port,
        ..Default::default()
    };
    let runtime = build_runtime(&config).map_err(|e| format!("runtime: {e}"))?;

    runtime.block_on(async move {
        let server = Server::start(room, config)
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
