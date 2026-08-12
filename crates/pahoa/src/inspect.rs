//! `pahoa inspect` — summarize a multidata file.
//!
//! Output is deliberately stable and machine-diffable: `tools/inspect-multidata.py`
//! produces the identical text from CPython, and the two are compared over every
//! fixture. That comparison is what makes the typed loader trustworthy — it
//! exercises slot typing, the location table, hints, versions and the data
//! package all at once, against an independent implementation.

use pahoa_multidata::{MultiData, SlotType};
use std::collections::BTreeMap;
use std::path::Path;

pub fn run(path: &Path, snapshot: Option<&Path>) -> Result<(), String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let md = MultiData::parse(&raw).map_err(|e| format!("{}: {e}", path.display()))?;

    let snapshot_games = match snapshot {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            pahoa_multidata::DataPackage::load_snapshot(&text)
                .map_err(|e| format!("{}: {e}", p.display()))?
        }
        None => BTreeMap::new(),
    };
    let (dp, report) = md.resolve_datapackage(&snapshot_games);

    println!("seed_name: {}", md.seed_name);
    println!("generator_version: {}", md.generator_version);
    println!("minimum_server_version: {}", md.minimum_server_version);
    println!("race_mode: {}", md.race_mode);

    // Counts by slot type, so a spectator or item-link group showing up in the
    // wrong bucket is immediately visible.
    let mut players = 0usize;
    let mut spectators = 0usize;
    let mut groups = 0usize;
    for s in md.slot_info.values() {
        match s.slot_type {
            SlotType::Player => players += 1,
            SlotType::Spectator => spectators += 1,
            SlotType::Group => groups += 1,
        }
    }
    println!("slots: {}", md.slot_info.len());
    println!("slots_player: {players}");
    println!("slots_spectator: {spectators}");
    println!("slots_group: {groups}");

    println!("connect_names: {}", md.connect_names.len());
    println!("locations_total: {}", md.locations.len());
    println!("locations_max_slot: {}", md.locations.max_slot());
    println!("spheres: {}", md.spheres.len());

    let precollected: usize = md.precollected_items.values().map(Vec::len).sum();
    println!("precollected_items: {precollected}");
    let hints: usize = md.precollected_hints.values().map(Vec::len).sum();
    println!("precollected_hints: {hints}");
    let er: usize = md.er_hint_data.values().map(|m| m.len()).sum();
    println!("er_hint_data: {er}");
    println!("slot_data_slots: {}", md.slot_data.len());
    println!("server_options: {}", md.server_options.is_some());

    println!("games: {}", dp.len());
    println!("datapackage_embedded: {}", md.embedded_datapackage.len());
    println!(
        "datapackage_from_multidata: {}",
        report.from_multidata.len()
    );
    println!("datapackage_from_snapshot: {}", report.from_snapshot.len());
    println!("datapackage_unresolved: {}", report.unresolved.len());

    // Per-game detail, sorted, so a name-table regression shows up as a diff on
    // one line rather than a changed total.
    for (game, names) in dp.games() {
        println!(
            "game {game}: items={} locations={} item_groups={} location_groups={} blacklist={} checksum={}",
            names.package.item_name_to_id.len(),
            names.package.location_name_to_id.len(),
            names.package.item_name_groups.len(),
            names.package.location_name_groups.len(),
            names.package.hint_blacklist.len(),
            names.package.checksum.as_deref().unwrap_or("-"),
        );
    }

    // Per-slot detail. Location counts come from the store, so this also
    // verifies the flat index lines up with slot_info.
    for (slot, info) in &md.slot_info {
        let kind = match info.slot_type {
            SlotType::Player => "player",
            SlotType::Spectator => "spectator",
            SlotType::Group => "group",
        };
        let members = info
            .group_members
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "slot {slot}: kind={kind} name={} game={} locations={} min_client={} members=[{members}]",
            info.name,
            info.game,
            md.locations.count_for(*slot),
            md.min_client_version(*slot),
        );
    }

    Ok(())
}
