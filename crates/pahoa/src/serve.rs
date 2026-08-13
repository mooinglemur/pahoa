//! `pahoa serve` — host a multiworld.

use pahoa_multidata::{DataPackage, GamePackage, MultiData};
use pahoa_net::actor::SaveConfig;
use pahoa_net::{NetConfig, SaveStore, Server, build_runtime};
use pahoa_pickle::PyObj;
use pahoa_proto::Permission;
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
    /// Where the room persists itself. `None` runs without saving at all, which
    /// is fine for a throwaway room and a data-loss bug for anything else.
    pub save_dir: Option<&'a Path>,
    pub save_interval: Duration,
    pub options: RoomOptions,
    /// Let the seed's own `server_options` override the options above.
    pub use_embedded_options: bool,
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

    let mut options = args.options;
    if args.use_embedded_options {
        apply_embedded(&mut options, data.server_options.as_ref());
    }
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

/// Overlay the seed's own `server_options` onto what the command line asked for.
///
/// The direction looks backwards and is the reference's: `Context.__init__`
/// takes the command-line values and `Context.load` applies the embedded ones
/// over the top (`MultiServer.py:558-560`), so the seed wins. That is what the
/// flag is *for* — honoring what the generator was configured with rather than
/// what whoever restarts the room happens to type.
///
/// Unrecognized keys are ignored in silence, because every real seed carries a
/// pile of them: `host`, `port`, `savefile`, `loglevel` and `auto_shutdown` are
/// the standalone server's own startup settings and mean nothing to a room that
/// was told where to listen on its command line. A key pahoa *does* implement
/// but cannot use warns instead, matching the reference's "Could not set server
/// option, skipping" (`:785-791`).
fn apply_embedded(options: &mut RoomOptions, server_options: Option<&PyObj>) {
    let Some(dict) = server_options.and_then(PyObj::as_dict) else {
        eprintln!("warning: --use-embedded-options, but this seed carries no server_options");
        return;
    };

    let mut applied: Vec<String> = Vec::new();
    for (key, raw) in dict {
        let Some(key) = key.as_str() else { continue };
        let taken = match key {
            "password" => text(raw).map(|v| {
                options.password = v;
                format!("password={}", shown(&options.password))
            }),
            "server_password" => text(raw).map(|v| {
                options.server_password = v;
                format!("server_password={}", shown(&options.server_password))
            }),
            "hint_cost" => count(raw).map(|v| {
                options.hint_cost = v;
                format!("hint_cost={v}")
            }),
            "location_check_points" => count(raw).map(|v| {
                options.location_check_points = v;
                format!("location_check_points={v}")
            }),
            "compatibility" => raw.as_int().filter(|v| (0..=2i64).contains(v)).map(|v| {
                options.compatibility = v as u8;
                format!("compatibility={v}")
            }),
            "release_mode" => permission(raw).map(|v| {
                options.release_mode = v;
                format!("release_mode={}", v.as_text())
            }),
            "collect_mode" => permission(raw).map(|v| {
                options.collect_mode = v;
                format!("collect_mode={}", v.as_text())
            }),
            "remaining_mode" => permission(raw).map(|v| {
                options.remaining_mode = v;
                format!("remaining_mode={}", v.as_text())
            }),
            "countdown_mode" => permission(raw).map(|v| {
                options.countdown_mode = v;
                format!("countdown_mode={}", v.as_text())
            }),
            "item_cheat" => truthy(raw).map(|v| {
                options.item_cheat = v;
                format!("item_cheat={v}")
            }),
            // Reported under the field it sets, not the key it arrived as —
            // `disable_item_cheat=false` reads like the opposite of what it did.
            "disable_item_cheat" => truthy(raw).map(|v| {
                options.item_cheat = !v;
                format!("item_cheat={}", !v)
            }),
            _ => continue,
        };
        match taken {
            Some(line) => applied.push(line),
            None => eprintln!(
                "warning: ignoring embedded server option {key}: unusable {}",
                raw.type_name()
            ),
        }
    }

    if applied.is_empty() {
        eprintln!("warning: --use-embedded-options, but this seed sets no room options");
    } else {
        println!("room options from the seed: {}", applied.join(" "));
    }
}

/// A string option, where `None` and `False` both mean "unset" — the
/// reference's coercion step skips exactly those (`MultiServer.py:784`), which
/// is how a seed spells "no password" rather than a password of `"None"`.
fn text(v: &PyObj) -> Option<Option<String>> {
    match v {
        PyObj::None | PyObj::Bool(false) => Some(None),
        PyObj::Str(s) => Some(Some(s.to_string())),
        _ => None,
    }
}

/// Whether a password is set, never which one: this line goes to a log an
/// operator may well paste somewhere.
fn shown(v: &Option<String>) -> &'static str {
    if v.is_some() { "set" } else { "none" }
}

fn count(v: &PyObj) -> Option<u32> {
    v.as_int().and_then(|n| u32::try_from(n).ok())
}

/// A mode, rejecting words the room would otherwise ignore in silence.
///
/// [`Permission::from_text`] is a substring test, so anything unrecognized
/// lands on `disabled` — a seed with a typo would quietly turn releases off.
/// Trusting it only when the word round-trips turns that into a warning.
fn permission(v: &PyObj) -> Option<Permission> {
    let text = v.as_str()?;
    let parsed = Permission::from_text(text);
    (parsed.as_text() == text.replace('_', "-")).then_some(parsed)
}

/// Python truthiness, over the shapes a boolean option actually arrives in.
fn truthy(v: &PyObj) -> Option<bool> {
    match v {
        PyObj::Bool(b) => Some(*b),
        PyObj::Int(n) => Some(*n != 0),
        PyObj::None => Some(false),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, PyObj)]) -> PyObj {
        PyObj::Dict(
            pairs
                .iter()
                .map(|(k, v)| (PyObj::Str((*k).into()), v.clone()))
                .collect(),
        )
    }

    fn s(v: &str) -> PyObj {
        PyObj::Str(v.into())
    }

    /// The exact key set every fixture in `crates/pahoa-pickle/tests/fixtures`
    /// carries, values from `AP_56807069331869547085`.
    ///
    /// Two of them differ from pahoa's defaults — `hint_cost` is 20 against a
    /// default of 10, and `collect_mode` is `disabled` against `auto` — so this
    /// is also the check that the overlay does anything at all.
    #[test]
    fn a_real_seeds_options_are_applied() {
        let mut o = RoomOptions::default();
        apply_embedded(
            &mut o,
            Some(&dict(&[
                ("host", PyObj::None),
                ("port", PyObj::Int(38281)),
                ("password", PyObj::None),
                ("multidata", PyObj::None),
                ("savefile", PyObj::None),
                ("disable_save", PyObj::Bool(false)),
                ("loglevel", s("info")),
                ("logtime", PyObj::Bool(false)),
                ("server_password", PyObj::None),
                ("disable_item_cheat", PyObj::Bool(false)),
                ("location_check_points", PyObj::Int(1)),
                ("hint_cost", PyObj::Int(20)),
                ("release_mode", s("auto")),
                ("collect_mode", s("disabled")),
                ("remaining_mode", s("goal")),
                ("countdown_mode", s("auto")),
                ("auto_shutdown", PyObj::Int(0)),
                ("compatibility", PyObj::Int(2)),
                ("log_network", PyObj::Int(0)),
            ])),
        );

        assert_eq!(o.hint_cost, 20);
        assert_eq!(o.location_check_points, 1);
        assert_eq!(o.release_mode, Permission::Auto);
        assert_eq!(o.collect_mode, Permission::Disabled);
        assert_eq!(o.remaining_mode, Permission::Goal);
        assert_eq!(o.countdown_mode, Permission::Auto);
        assert_eq!(o.compatibility, 2);
        assert!(o.item_cheat);
        assert!(o.password.is_none() && o.server_password.is_none());
    }

    #[test]
    fn the_seed_overrides_the_command_line() {
        let mut o = RoomOptions {
            hint_cost: 5,
            password: Some("from-the-flag".to_string()),
            ..Default::default()
        };
        apply_embedded(
            &mut o,
            Some(&dict(&[
                ("hint_cost", PyObj::Int(20)),
                ("password", s("from-the-seed")),
            ])),
        );
        assert_eq!(o.hint_cost, 20);
        assert_eq!(o.password.as_deref(), Some("from-the-seed"));
    }

    #[test]
    fn options_the_seed_omits_keep_their_command_line_value() {
        let mut o = RoomOptions {
            hint_cost: 5,
            ..Default::default()
        };
        apply_embedded(&mut o, Some(&dict(&[("release_mode", s("goal"))])));
        assert_eq!(o.hint_cost, 5);
        assert_eq!(o.release_mode, Permission::Goal);
    }

    #[test]
    fn an_unrecognized_mode_is_refused_rather_than_becoming_disabled() {
        // `Permission::from_text` is a substring test and would answer
        // `disabled` here, turning releases off room-wide on a typo.
        let mut o = RoomOptions {
            release_mode: Permission::Enabled,
            ..Default::default()
        };
        apply_embedded(&mut o, Some(&dict(&[("release_mode", s("enable"))])));
        assert_eq!(o.release_mode, Permission::Enabled);
    }

    #[test]
    fn auto_enabled_is_accepted_spelled_either_way() {
        for spelling in ["auto-enabled", "auto_enabled"] {
            let mut o = RoomOptions::default();
            apply_embedded(&mut o, Some(&dict(&[("collect_mode", s(spelling))])));
            assert_eq!(o.collect_mode, Permission::AutoEnabled, "{spelling}");
        }
    }

    #[test]
    fn disable_item_cheat_inverts_the_field_it_sets() {
        let mut o = RoomOptions::default();
        assert!(o.item_cheat);
        apply_embedded(
            &mut o,
            Some(&dict(&[("disable_item_cheat", PyObj::Bool(true))])),
        );
        assert!(!o.item_cheat);
    }

    /// How a seed spells "no password" — not a password of `"None"`.
    #[test]
    fn none_and_false_clear_a_password_rather_than_setting_one() {
        for empty in [PyObj::None, PyObj::Bool(false)] {
            let mut o = RoomOptions {
                password: Some("from-the-flag".to_string()),
                ..Default::default()
            };
            apply_embedded(&mut o, Some(&dict(&[("password", empty.clone())])));
            assert!(o.password.is_none(), "{empty:?}");
        }
    }

    #[test]
    fn a_seed_without_server_options_changes_nothing() {
        let mut o = RoomOptions {
            hint_cost: 7,
            ..Default::default()
        };
        apply_embedded(&mut o, None);
        assert_eq!(o.hint_cost, 7);
    }
}
