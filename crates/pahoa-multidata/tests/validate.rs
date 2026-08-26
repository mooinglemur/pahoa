//! The load-time checks, against real seeds.
//!
//! `MultiData::validate` gates whether a room starts at all, so a check that is
//! subtly too strict does not fail a unit test — it fails somebody's
//! multiworld. Every seed in the fixture corpus has to pass it.
//!
//! Fixtures are gitignored symlinks; see `crates/pahoa-pickle/tests/fixtures.rs`
//! for how to set them up. With none present this skips loudly.

use pahoa_multidata::{MultiData, Version};
use std::path::PathBuf;

/// Matches the version `pahoa_room::SERVER_VERSION` reports. Hardcoded rather
/// than imported so this crate keeps no dependency on the one above it.
const SERVER_VERSION: Version = Version::new(0, 6, 7);

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

fn fixtures() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(fixture_dir()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "archipelago"))
        .collect();
    paths.sort();
    paths
}

/// A seed naming a team this server cannot serve is refused at load.
///
/// The reference accepts the same seed and then raises inside
/// `ctx.clients[team][slot]` on the connect that used the name, with the room
/// already up. Refusing is the same limit, said where it can be acted on.
#[test]
fn a_seed_on_another_team_is_refused_rather_than_half_served() {
    use pahoa_multidata::{LocationEntry, LocationStore, NetworkSlot, SlotType};
    use std::collections::{BTreeMap, HashMap};

    // A table that passes its own checks, so what fails below is the team and
    // not the locations.
    let locations = || LocationStore::from_entries(vec![LocationEntry::new(1, 10, 77, 1, 0)], 1);

    let mut slot_info = BTreeMap::new();
    slot_info.insert(
        1,
        NetworkSlot {
            name: "Troy".to_string(),
            game: "A Link to the Past".to_string(),
            slot_type: SlotType::Player,
            group_members: Vec::new(),
        },
    );

    let seed = |team: u32| {
        let mut connect_names = HashMap::new();
        connect_names.insert("Troy".to_string(), (team, 1));
        MultiData {
            seed_name: "teams".to_string(),
            generator_version: Version::new(0, 6, 2),
            minimum_server_version: Version::new(0, 1, 6),
            minimum_client_versions: HashMap::new(),
            slot_info: slot_info.clone(),
            connect_names,
            locations: locations(),
            precollected_items: HashMap::new(),
            precollected_hints: HashMap::new(),
            er_hint_data: HashMap::new(),
            spheres: Vec::new(),
            race_mode: false,
            slot_data: HashMap::new(),
            server_options: None,
            embedded_datapackage: BTreeMap::new(),
        }
    };

    // The control: the same seed on team 0 is accepted, so what the assertion
    // below catches is the team rather than anything else about the shape.
    seed(pahoa_multidata::ONLY_TEAM)
        .validate(SERVER_VERSION)
        .expect("team 0 is the team this server serves");

    let refused = seed(1).validate(SERVER_VERSION).unwrap_err().to_string();
    assert!(
        refused.contains("team 1") && refused.contains("one team"),
        "should name the team and the limit: {refused}"
    );
}

#[test]
fn every_real_seed_passes_the_load_time_checks() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!(
            "SKIP: no .archipelago fixtures in {}",
            fixture_dir().display()
        );
        return;
    }

    for path in &fixtures {
        let raw = std::fs::read(path).expect("fixture readable");
        let data = MultiData::parse(&raw).expect("fixture parses");
        data.validate(SERVER_VERSION)
            .unwrap_or_else(|e| panic!("{} would be refused at load: {e}", path.display()));

        // And every one of them is the single team the reference generates.
        assert_eq!(
            data.teams().collect::<Vec<_>>(),
            vec![pahoa_multidata::ONLY_TEAM],
            "{}",
            path.display()
        );
    }
    eprintln!("{} seeds validated", fixtures.len());
}
