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
use tracing::level_filters::LevelFilter;

pub struct ServeArgs<'a> {
    pub multidata: &'a Path,
    pub snapshot: Option<&'a Path>,
    pub port: u16,
    pub bind: String,
    /// Where the room persists itself. `None` runs without saving at all, which
    /// is fine for a throwaway room and a data-loss bug for anything else.
    pub save_dir: Option<&'a Path>,
    pub save_interval: Duration,
    /// `None` derives it from the seed's slot count.
    pub outbound_budget_bytes: Option<usize>,
    pub options: RoomOptions,
    /// Let the seed's own `server_options` override the options above.
    pub use_embedded_options: bool,
    pub log_level: LevelFilter,
    /// Passwords, and where each came from. Applied over `options` and, for
    /// anything the environment supplied, protected from the seed.
    pub secrets: crate::secrets::Secrets,
    /// `None` serves plaintext only.
    pub tls: Option<pahoa_net::TlsPaths>,
    pub allow_plaintext: bool,
}

pub fn run(args: ServeArgs<'_>) -> Result<(), String> {
    init_logging(args.log_level);

    // Resolved before the subscriber existed, so they are said now.
    for warning in &args.secrets.warnings {
        tracing::warn!("{warning}");
    }

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
        tracing::warn!(
            games = report.unresolved.len(),
            "no data package for {}",
            report.unresolved.join(", ")
        );
    }
    if !report.missing_hint_blacklist.is_empty() {
        tracing::warn!(
            games = report.missing_hint_blacklist.len(),
            "no hint blacklist; !hint will not refuse non-hintable names for \
             them (export one with tools/export-datapackage.py)"
        );
    }

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    // Secrets first, so that a seed's embedded `server_options` can still
    // override one that came from a flag — and, in `apply_embedded`, cannot
    // override one that came from the environment.
    let mut options = args.options;
    options.password = args.secrets.password.clone();
    options.server_password = args.secrets.server_password.clone();
    options.slot_passwords = args.secrets.slot_passwords.clone();
    if args.use_embedded_options {
        apply_embedded(&mut options, data.server_options.as_ref(), &args.secrets);
    }
    let mut room = Room::new(data.clone(), Arc::new(names), options, start_time);

    // Claim the save directory and restore *before* binding, so a client can
    // never reach a half-loaded room, and so a directory another pod already
    // holds fails here rather than after we have started answering.
    let saves = match args.save_dir {
        None => {
            tracing::warn!("no --save-dir, so this room keeps nothing across a restart");
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
                    tracing::info!("restored {} from {}", plural(slots), store.path().display());
                }
                None => tracing::info!("no save in {}; starting fresh", dir.display()),
            }
            SaveConfig {
                store: Some(Arc::new(store)),
                interval: args.save_interval,
                ..Default::default()
            }
        }
    };

    // Sized from the seed rather than left at a constant: the cap is there to
    // survive clients that stop reading, and how much that is depends entirely
    // on how many of them there are.
    let budget = args
        .outbound_budget_bytes
        .unwrap_or_else(|| pahoa_net::outbound_budget_for(data.slot_info.len()));
    // Reported on their own lines rather than folded into the startup line,
    // which puna parses and which keeps its shape.
    if let Some(paths) = &args.tls {
        tracing::info!(
            cert = %paths.cert.display(),
            key = %paths.key.display(),
            "terminating TLS on the room port"
        );
        if args.allow_plaintext {
            tracing::warn!(
                "--allow-plaintext: this room also answers ws://, so anything sent \
                 over it — passwords, the admin token — is in the clear"
            );
        }
    }

    if args.secrets.admin_token.is_some() {
        tracing::info!("the admin API is enabled on /admin/v1/");
    }

    let config = NetConfig {
        bind: args.bind,
        port: args.port,
        outbound_budget_bytes: budget,
        tls: args.tls,
        allow_plaintext: args.allow_plaintext,
        admin_token: args.secrets.admin_token.clone(),
        ..Default::default()
    };
    let runtime = build_runtime(&config).map_err(|e| format!("runtime: {e}"))?;

    runtime.block_on(async move {
        let server = Server::start_with_saves(room, config, saves)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        // The one line on stdout, and the only machine-readable evidence a room
        // came up. The build version is appended rather than inserted so every
        // field that was already here keeps its position.
        println!(
            "pahoa serving {} slots, {} locations, seed {} on {} \
             (outbound budget {} MiB, version {})",
            data.slot_info.len(),
            data.locations.len(),
            data.seed_name,
            server.local_addr,
            budget / (1024 * 1024),
            env!("CARGO_PKG_VERSION"),
        );

        // Every way out of a running room converges here, so they all get the
        // same quiesce and the same final save.
        let reason = tokio::select! {
            signal = shutdown_signal() => signal,
            () = server.shutdown_requested() => "admin request",
        };
        tracing::info!(reason, "shutting down");
        server.shutdown().await;
        Ok(())
    })
}

/// Start collecting the `tracing` events the crates below this one emit.
///
/// Without a subscriber every one of them is discarded, which is how a room
/// whose saves are failing — `actor.rs` logs that at `error!` — could run
/// completely silently.
///
/// Logs go to **stderr**, which leaves stdout carrying only the startup line.
/// That is what makes `pahoa serve … 2>/dev/null` a way to read the one line a
/// machine is meant to parse.
fn init_logging(level: LevelFilter) {
    use std::io::IsTerminal;

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        // Color when a person is watching, plain text when the kubelet is.
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Resolve on the first signal asking this process to stop, naming it.
///
/// SIGTERM is the one that matters in a container: Kubernetes sends it and
/// SIGKILLs after the grace period, so a room waiting only on SIGINT never runs
/// `server.shutdown()` and silently loses up to `--save-interval` of play on
/// every teardown — a rollout, a node drain, a rescheduled pod.
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    // Failing to install the handler is not worth refusing to serve over. The
    // room still saves on its timer and SIGINT still works; say so and carry on.
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cannot handle SIGTERM; only SIGINT will stop this room cleanly"
            );
            tokio::signal::ctrl_c().await.ok();
            return "SIGINT";
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = term.recv() => "SIGTERM",
    }
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
fn apply_embedded(
    options: &mut RoomOptions,
    server_options: Option<&PyObj>,
    secrets: &crate::secrets::Secrets,
) {
    let Some(dict) = server_options.and_then(PyObj::as_dict) else {
        tracing::warn!("--use-embedded-options, but this seed carries no server_options");
        return;
    };

    let mut applied: Vec<String> = Vec::new();
    for (key, raw) in dict {
        let Some(key) = key.as_str() else { continue };
        let taken = match key {
            // The seed outranks the command line, but not the environment. A
            // password baked in at generation time is readable by anyone
            // holding the seed, and letting it shadow the configured one is the
            // same failure as persisting a password into `room.save`: rotation
            // appears to work and then reverts.
            "password" => {
                if secrets.password_from_env {
                    tracing::warn!(
                        "the seed sets a room password, but one is configured in the \
                         environment and wins; ignoring the seed's"
                    );
                    continue;
                }
                if !secrets.slot_passwords.is_empty() {
                    tracing::warn!(
                        "the seed sets a room password, but this room is in per-slot \
                         password mode; ignoring the seed's"
                    );
                    continue;
                }
                text(raw).map(|v| {
                    options.password = v;
                    format!("password={}", shown(&options.password))
                })
            }
            "server_password" => {
                if secrets.server_password_from_env {
                    tracing::warn!(
                        "the seed sets a server password, but one is configured in the \
                         environment and wins; ignoring the seed's"
                    );
                    continue;
                }
                text(raw).map(|v| {
                    options.server_password = v;
                    format!("server_password={}", shown(&options.server_password))
                })
            }
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
            None => tracing::warn!(
                "ignoring embedded server option {key}: unusable {}",
                raw.type_name()
            ),
        }
    }

    if applied.is_empty() {
        tracing::warn!("--use-embedded-options, but this seed sets no room options");
    } else {
        tracing::info!("room options from the seed: {}", applied.join(" "));
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
            tracing::warn!(
                "reading the save in {} is taking a long time. If this is a \
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

    /// Apply a seed's options with nothing configured against them, which is
    /// the case for every test that is not specifically about precedence.
    fn embed(options: &mut RoomOptions, server_options: Option<&PyObj>) {
        apply_embedded(options, server_options, &crate::secrets::Secrets::default());
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
        embed(
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
        embed(
            &mut o,
            Some(&dict(&[
                ("hint_cost", PyObj::Int(20)),
                ("password", s("from-the-seed")),
            ])),
        );
        assert_eq!(o.hint_cost, 20);
        assert_eq!(o.password.as_deref(), Some("from-the-seed"));
    }

    /// ...but not over the environment. Precedence is environment, then seed,
    /// then argv.
    ///
    /// A password baked into a seed at generation time is readable by anyone
    /// holding the seed, and letting it win would make the configured password
    /// silently not the one in force — the same class of bug as persisting a
    /// password into `room.save`, where rotation appears to work and reverts.
    #[test]
    fn the_environment_overrides_even_the_seed() {
        let mut o = RoomOptions {
            password: Some("from-the-environment".to_string()),
            server_password: Some("admin-from-the-environment".to_string()),
            ..Default::default()
        };
        apply_embedded(
            &mut o,
            Some(&dict(&[
                ("hint_cost", PyObj::Int(20)),
                ("password", s("from-the-seed")),
                ("server_password", s("admin-from-the-seed")),
            ])),
            &crate::secrets::Secrets {
                password_from_env: true,
                server_password_from_env: true,
                ..Default::default()
            },
        );
        assert_eq!(o.password.as_deref(), Some("from-the-environment"));
        assert_eq!(
            o.server_password.as_deref(),
            Some("admin-from-the-environment")
        );
        // Non-secret options are unaffected — the seed still wins those.
        assert_eq!(o.hint_cost, 20);
    }

    /// A seed carrying a room-wide password must not quietly re-enable that
    /// mode in a room configured for per-slot passwords.
    #[test]
    fn a_seed_password_is_ignored_in_per_slot_mode() {
        let mut o = RoomOptions::default();
        let mut secrets = crate::secrets::Secrets::default();
        secrets.slot_passwords.insert(1, "per-slot".to_string());

        apply_embedded(
            &mut o,
            Some(&dict(&[("password", s("from-the-seed"))])),
            &secrets,
        );
        assert_eq!(
            o.password, None,
            "the seed must not set a room password here"
        );
    }

    #[test]
    fn options_the_seed_omits_keep_their_command_line_value() {
        let mut o = RoomOptions {
            hint_cost: 5,
            ..Default::default()
        };
        embed(&mut o, Some(&dict(&[("release_mode", s("goal"))])));
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
        embed(&mut o, Some(&dict(&[("release_mode", s("enable"))])));
        assert_eq!(o.release_mode, Permission::Enabled);
    }

    #[test]
    fn auto_enabled_is_accepted_spelled_either_way() {
        for spelling in ["auto-enabled", "auto_enabled"] {
            let mut o = RoomOptions::default();
            embed(&mut o, Some(&dict(&[("collect_mode", s(spelling))])));
            assert_eq!(o.collect_mode, Permission::AutoEnabled, "{spelling}");
        }
    }

    #[test]
    fn disable_item_cheat_inverts_the_field_it_sets() {
        let mut o = RoomOptions::default();
        assert!(o.item_cheat);
        embed(
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
            embed(&mut o, Some(&dict(&[("password", empty.clone())])));
            assert!(o.password.is_none(), "{empty:?}");
        }
    }

    #[test]
    fn a_seed_without_server_options_changes_nothing() {
        let mut o = RoomOptions {
            hint_cost: 7,
            ..Default::default()
        };
        embed(&mut o, None);
        assert_eq!(o.hint_cost, 7);
    }
}
