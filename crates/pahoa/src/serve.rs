//! `pahoa serve` — host a multiworld.

use pahoa_multidata::MultiData;
use pahoa_net::actor::SaveConfig;
use pahoa_net::{NetConfig, SaveStore, Server, build_runtime};
use pahoa_pickle::PyObj;
use pahoa_proto::Permission;
use pahoa_room::{Room, RoomOptions, Snapshot};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::level_filters::LevelFilter;

pub struct ServeArgs<'a> {
    pub multidata: &'a Path,
    pub port: u16,
    pub bind: String,
    /// Where the room persists itself. `None` runs without saving at all, which
    /// is fine for a throwaway room and a data-loss bug for anything else.
    pub save_dir: Option<&'a Path>,
    pub save_interval: Duration,
    /// WebSocket keepalive cadence and answer deadline. Zero disables either.
    pub ping_interval: Duration,
    pub ping_timeout: Duration,
    /// `None` derives it from the seed's slot count.
    pub outbound_budget_bytes: Option<usize>,
    /// Fan-out width and how deep each shard's frame inbox is. `None` derives
    /// both from the seed's slot count, and the second from the first.
    ///
    /// Passed explicitly by an orchestrator for the same reason
    /// `outbound_budget_bytes` is: it sizes the container against these, so the
    /// value it sized for and the value the room runs at must not be able to
    /// disagree.
    pub shards: Option<usize>,
    pub shard_queue_depth: Option<usize>,
    pub options: RoomOptions,
    /// Which room-option flags were actually given, as their flag spellings.
    ///
    /// Only used to notice that a restored save is about to overrule one. A
    /// flag that was not given cannot be overruled — its value *is* the
    /// default — so warning without this would fire on every restart of every
    /// room whose options are not all default.
    pub explicit_options: Vec<&'static str>,
    /// Append a durable per-check history to the save directory.
    pub journal: bool,
    /// Let the seed's own `server_options` override the options above.
    pub use_embedded_options: bool,
    pub log_level: LevelFilter,
    pub log_format: LogFormat,
    /// The command line, with secret values replaced. Reported in the banner.
    pub argv: String,
    /// Passwords, and where each came from. Applied over `options` and, for
    /// anything the environment supplied, protected from the seed.
    pub secrets: crate::secrets::Secrets,
    /// `None` serves plaintext only.
    pub tls: Option<pahoa_net::TlsPaths>,
    pub allow_plaintext: bool,
    /// Serve the tracker unauthenticated even with an admin token configured.
    pub open_tracker: bool,
    /// A second port serving the scoped feed.
    pub filtered_port: Option<u16>,
}

pub fn run(args: ServeArgs<'_>) -> Result<(), String> {
    init_logging(args.log_level, args.log_format);
    banner(&args);

    // Resolved before the subscriber existed, so they are said now.
    for warning in &args.secrets.warnings {
        tracing::warn!("{warning}");
    }

    let raw =
        std::fs::read(args.multidata).map_err(|e| format!("{}: {e}", args.multidata.display()))?;
    let data =
        Arc::new(MultiData::parse(&raw).map_err(|e| format!("{}: {e}", args.multidata.display()))?);

    // The consistency checks the reference runs at load, run at load. A seed
    // that demands a newer server, points a connect name at a slot that does
    // not exist, or names a team this server cannot serve is refused here —
    // before the port binds, so an orchestrator sees a room that never came up
    // rather than one that came up and then behaved oddly.
    data.validate(pahoa_room::SERVER_VERSION.into())
        .map_err(|e| format!("{}: {e}", args.multidata.display()))?;

    let (names, report) = data.resolve_datapackage();
    if !report.unresolved.is_empty() {
        // Not fatal — names degrade to "Unknown item (ID:n)" — but an operator
        // should be told rather than left to notice in chat.
        tracing::warn!(
            games = report.unresolved.len(),
            "no data package for {}",
            report.unresolved.join(", ")
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
                    let asked_for = room.options.clone();
                    room.restore(snapshot)
                        .map_err(|e| format!("{}: {e}", store.path().display()))?;
                    tracing::info!("restored {} from {}", plural(slots), store.path().display());
                    for line in overruled_options(&args.explicit_options, &asked_for, &room.options)
                    {
                        tracing::warn!("{line}");
                    }
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

    // Opened after the restore, so a room that fails to load never appends to
    // the history of the one it failed to become.
    let (journal, journal_writer) = match (args.journal, args.save_dir) {
        (false, _) => (None, None),
        (true, None) => {
            return Err(
                "--journal needs --save-dir: the history is kept beside the save, so that \
                 it survives a restart the way the save does"
                    .to_string(),
            );
        }
        (true, Some(dir)) => {
            let (journal, writer) = pahoa_net::journal::Journal::open(
                dir,
                data.clone(),
                Arc::clone(room.datapackage()),
            )
            .map_err(|e| format!("journal in {}: {e}", dir.display()))?;
            tracing::info!(path = %writer.path().display(), "appending a per-check history");
            // The rules this room is starting under, before anything happens in
            // it. Written on every start rather than only the first, because a
            // restart is exactly when they can have changed — and a reader
            // scanning back from any point finds the options in force without
            // replaying every change from the beginning.
            journal.event(pahoa_room::JournalEvent::options(start_time, &room.options));
            (Some(journal), Some(writer))
        }
    };
    let saves = SaveConfig { journal, ..saves };

    // Sized from the seed rather than left at a constant: the cap is there to
    // survive clients that stop reading, and how much that is depends entirely
    // on how many of them there are.
    let budget = args
        .outbound_budget_bytes
        .unwrap_or_else(|| pahoa_net::outbound_budget_for(data.slot_info.len()));
    // Sized from the seed for the same reason, and the depth from the width:
    // what a shard must absorb is a burst from the connections it owns, so
    // halving the fan-out doubles what each shard needs to hold. Deriving the
    // second from whichever width is actually in force — flag or default —
    // means passing only `--shards` still resizes both.
    let shards = args
        .shards
        .unwrap_or_else(|| pahoa_net::shards_for(data.slot_info.len()));
    let shard_queue_depth = args
        .shard_queue_depth
        .unwrap_or_else(|| pahoa_net::shard_queue_depth_for(data.slot_info.len(), shards));
    // Reported on their own lines rather than folded into the startup line,
    // which puna parses and which keeps its shape.
    tracing::info!(
        shards,
        shard_queue_depth,
        // What an overflowing shard would close, which is the number worth
        // looking at when `pahoa_shard_overflow_total` moves.
        blast_radius = data.slot_info.len().saturating_mul(3).div_ceil(shards),
        // Not inside `outbound_budget_bytes` — that is charged when a frame is
        // queued for a connection, downstream of these queues — so anything
        // sizing a container against the budget has to add this.
        queue_bytes = pahoa_net::shard_queue_bytes(shards, shard_queue_depth),
        "fanning out"
    );
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
        shards: Some(shards),
        shard_queue_depth: Some(shard_queue_depth),
        ping_interval: args.ping_interval,
        ping_timeout: args.ping_timeout,
        tls: args.tls,
        allow_plaintext: args.allow_plaintext,
        open_tracker: args.open_tracker,
        filtered_port: args.filtered_port,
        admin_token: args.secrets.admin_token.clone(),
        ..Default::default()
    };
    let runtime = build_runtime(&config).map_err(|e| format!("runtime: {e}"))?;

    let result = runtime.block_on(async move {
        let server = Server::start_with_saves(room, config, saves)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        // The one line on stdout, and the only machine-readable evidence a room
        // came up. The build version is appended rather than inserted so every
        // field that was already here keeps its position.
        // One announcement, in whichever shape this room's reader can parse.
        //
        // Under `text` it is the historical stdout line, unchanged: logs are
        // prose there, so a dedicated stream carrying exactly one machine-
        // readable line is the only way to make "the room came up" parseable.
        //
        // Under `json` that reasoning inverts. A container merges stdout and
        // stderr into one pod log, so the plain line would be a single
        // unparseable entry in a stream of objects — every room, forever — and
        // the log is now structured, so the dedicated stream buys nothing that
        // the event stream does not already give. Emitting both was the earlier
        // answer and was worse: two records of one fact, which anything
        // counting room starts has to know to de-duplicate.
        match args.log_format {
            LogFormat::Text => println!(
                "pahoa serving {} slots, {} locations, seed {} on {} \
                 (outbound budget {} MiB, version {})",
                data.slot_info.len(),
                data.locations.len(),
                data.seed_name,
                server.local_addr,
                budget / (1024 * 1024),
                env!("CARGO_PKG_VERSION"),
            ),
            // Self-contained on purpose: `version` and `build_rev` repeat what
            // the banner said, so that matching this one event is enough to
            // answer "which build came up serving what, where".
            LogFormat::Json => tracing::info!(
                slots = data.slot_info.len(),
                locations = data.locations.len(),
                seed_name = %data.seed_name,
                addr = %server.local_addr,
                outbound_budget_bytes = budget,
                version = env!("CARGO_PKG_VERSION"),
                build_rev = env!("PAHOA_BUILD_REV"),
                "serving"
            ),
        }

        // Every way out of a running room converges here, so they all get the
        // same quiesce and the same final save.
        if let Some(addr) = server.filtered_addr {
            tracing::info!(%addr, "serving the scoped feed");
        }

        let reason = tokio::select! {
            signal = shutdown_signal() => signal,
            () = server.shutdown_requested() => "admin request",
        };
        tracing::info!(reason, "shutting down");
        server.shutdown().await;
        Ok(())
    });

    // After the runtime is done, so every `Journal` clone the actor held has
    // been dropped and the writer's channel has closed. Joining before that
    // would wait forever; skipping it would end the process with the last
    // checks still in a buffer.
    if let Some(writer) = journal_writer {
        writer.finish();
    }
    result
}

/// Start collecting the `tracing` events the crates below this one emit.
///
/// Without a subscriber every one of them is discarded, which is how a room
/// whose saves are failing — `actor.rs` logs that at `error!` — could run
/// completely silently.
///
/// Logs go to **stderr**, which under `--log-format text` leaves stdout
/// carrying only the startup line — what makes `pahoa serve … 2>/dev/null` a
/// way to read the one line a machine is meant to parse. Under `json` there is
/// no stdout line at all and the whole stream is on stderr, because a
/// structured log needs no separate channel to be parseable.
/// The build, the invocation and the machine, as the first event logged.
///
/// Everything here answers a question asked *after* something has gone wrong,
/// when the room in question is gone and all that survives is its log. The
/// revision matters most: `0.1.0` will be every build for months, so a version
/// alone cannot tell two rooms apart, and `+` marks a binary built from a tree
/// that did not match any commit.
///
/// The structured fields are the reason `--log-format json` is worth having —
/// under it every one of these becomes a queryable key rather than something to
/// pull back out of a message with a regex.
fn banner(args: &ServeArgs<'_>) {
    // Every optional field is an `Option`, which `tracing` omits entirely when
    // it is `None` rather than recording an empty string. That is what keeps a
    // hand-run room's banner free of `pod=""` and, under `--log-format json`,
    // keeps a limit a *number* instead of the string "none" for whatever ends
    // up querying it.
    let host = hostname();
    // Kubernetes supplies none of these on its own; they are downward-API
    // values an orchestrator chooses to pass, so their presence is itself the
    // signal that this room is orchestrated.
    let (pod, namespace, node) = (
        env_field("POD_NAME"),
        env_field("POD_NAMESPACE"),
        env_field("NODE_NAME"),
    );

    tracing::info!(
        argv = %args.argv,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
        host = host.as_deref(),
        pod = pod.as_deref(),
        namespace = namespace.as_deref(),
        node = node.as_deref(),
        // The sizing inputs together, because the failure they explain is a
        // room given fewer CPUs than the node advertises. An absent quota means
        // no cgroup cap, which is why it is missing rather than zero.
        worker_threads = pahoa_net::detect_worker_threads(),
        cpu_quota = pahoa_net::cgroup_cpu_quota(),
        host_cpus = std::thread::available_parallelism().map(|n| n.get()).ok(),
        memory_limit_bytes = pahoa_net::cgroup_memory_limit(),
        "Pahoa-{}-{} starting",
        env!("CARGO_PKG_VERSION"),
        env!("PAHOA_BUILD_REV"),
    );
}

/// This machine's name, which under Kubernetes is the pod's name.
///
/// From `/proc` rather than `libc::gethostname`, so this stays dependency-free
/// and static. `$HOSTNAME` is not equivalent: it is a shell convention, and a
/// container started without one has it unset.
fn hostname() -> Option<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// An environment variable, absent when unset *or* set to nothing.
///
/// The empty case matters: a manifest that declares a downward-API variable
/// whose source does not resolve supplies an empty string rather than omitting
/// it, and `pod=""` is worse than no field at all.
fn env_field(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// How log lines are rendered.
///
/// Deliberately **not** inferred from `stderr().is_terminal()`, though the
/// colouring right below it is. Colour is cosmetic and getting it wrong costs
/// nothing; the format is structural, and inferring it would mean
/// `pahoa serve … 2>debug.log` or piping through `less` silently produced a
/// different shape than the same command produced on the screen. An operator
/// choosing to redirect is not thereby asking for JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One human-readable line per event. The default: a standalone room is run
    /// from a terminal and read by the person who started it.
    Text,
    /// One JSON object per line, fields as keys. For anything that ships logs
    /// somewhere queryable, where re-parsing `games=54` out of a message with a
    /// regex is the failure this avoids.
    Json,
}

fn init_logging(level: LevelFilter, format: LogFormat) {
    use std::io::IsTerminal;

    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr);

    match format {
        // Colour when a person is watching, plain text when the kubelet is.
        LogFormat::Text => builder.with_ansi(std::io::stderr().is_terminal()).init(),
        // `flatten_event` lifts the event's own fields to the top level rather
        // than nesting them under "fields", so a query is `.slot` and not
        // `.fields.slot`. `current_span`/`span_list` are off because pahoa
        // opens no spans; they would be two empty keys on every line.
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .init(),
    }
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

/// Say when a restored save has overruled a flag that was actually given.
///
/// The save winning is correct and deliberate — it is what lets `!admin
/// /option` mean anything past the next restart — but it is the mirror of the
/// bug that made passwords non-persistent, and silently ignoring a flag someone
/// typed is how that bug went unnoticed for as long as it did. An operator who
/// edits `--hint-cost` in a manifest, redeploys, and sees nothing change is owed
/// an explanation rather than a debugging session.
///
/// Only flags actually supplied are compared. A flag left off holds the default,
/// and "the save replaced the default" is every restart of every room that has
/// ever changed an option.
///
/// Returns the lines rather than logging them so the decision can be tested;
/// which flags count as *supplied* is the part that was wrong first time, and it
/// is invisible from outside.
fn overruled_options(
    explicit: &[&str],
    asked_for: &RoomOptions,
    restored: &RoomOptions,
) -> Vec<String> {
    fn cheat_state(on: bool) -> String {
        if on { "enabled" } else { "disabled" }.to_string()
    }

    // Flag spelling, what was asked for, what the save had. Rendered eagerly
    // because a `Permission` and a `u32` do not share a type.
    let compared: [(&str, String, String); 8] = [
        (
            "--hint-cost",
            asked_for.hint_cost.to_string(),
            restored.hint_cost.to_string(),
        ),
        (
            "--location-check-points",
            asked_for.location_check_points.to_string(),
            restored.location_check_points.to_string(),
        ),
        (
            "--release-mode",
            asked_for.release_mode.as_text().to_string(),
            restored.release_mode.as_text().to_string(),
        ),
        (
            "--collect-mode",
            asked_for.collect_mode.as_text().to_string(),
            restored.collect_mode.as_text().to_string(),
        ),
        (
            "--remaining-mode",
            asked_for.remaining_mode.as_text().to_string(),
            restored.remaining_mode.as_text().to_string(),
        ),
        (
            "--countdown-mode",
            asked_for.countdown_mode.as_text().to_string(),
            restored.countdown_mode.as_text().to_string(),
        ),
        (
            // Rendered as the cheat's state rather than the flag's boolean: a
            // negative flag reporting "asked for false, save says true" makes a
            // reader work out which way round it goes.
            "--no-item-cheat",
            cheat_state(asked_for.item_cheat),
            cheat_state(restored.item_cheat),
        ),
        (
            "--compatibility",
            asked_for.compatibility.to_string(),
            restored.compatibility.to_string(),
        ),
    ];

    compared
        .into_iter()
        .filter(|(flag, wanted, actual)| explicit.contains(flag) && wanted != actual)
        .map(|(flag, wanted, actual)| {
            format!(
                "{flag} asked for {wanted}, but the restored save says {actual} and wins; \
                 room options live in the save once one exists, and are changed with \
                 !admin /option or by starting from an empty --save-dir"
            )
        })
        .collect()
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
                if secrets.slot_passwords.is_some() {
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
        let secrets = crate::secrets::Secrets {
            slot_passwords: Some(std::collections::BTreeMap::from([(
                1,
                "per-slot".to_string(),
            )])),
            ..Default::default()
        };

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

    // --- a restored save overruling a flag -------------------------------

    #[test]
    fn a_save_overruling_a_flag_that_was_given_is_reported() {
        let asked = RoomOptions {
            hint_cost: 99,
            release_mode: Permission::Disabled,
            ..Default::default()
        };
        let restored = RoomOptions {
            hint_cost: 15,
            release_mode: Permission::Enabled,
            ..Default::default()
        };

        let lines = overruled_options(&["--hint-cost", "--release-mode"], &asked, &restored);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("--hint-cost asked for 99"), "{lines:?}");
        assert!(lines[0].contains("save says 15"), "{lines:?}");
        assert!(
            lines[1].contains("--release-mode asked for disabled"),
            "{lines:?}"
        );
    }

    /// The half that was wrong when this was first written, and that only
    /// showed up by running a real room: `Args::is_set` answers for bare flags
    /// and `Args::get` for `--option value` pairs, so testing the wrong one
    /// leaves `explicit` permanently empty and the warning permanently silent.
    #[test]
    fn a_flag_that_was_not_given_is_not_reported() {
        let asked = RoomOptions::default();
        let restored = RoomOptions {
            hint_cost: 15,
            ..Default::default()
        };
        assert!(
            overruled_options(&[], &asked, &restored).is_empty(),
            "a default nobody asked for was reported as overruled"
        );
    }

    #[test]
    fn a_flag_the_save_agrees_with_is_not_reported() {
        let same = RoomOptions {
            hint_cost: 15,
            ..Default::default()
        };
        assert!(overruled_options(&["--hint-cost"], &same, &same).is_empty());
    }

    /// A negative flag reads backwards as a raw boolean, so it reports the
    /// cheat's state rather than the flag's value.
    #[test]
    fn the_item_cheat_reports_its_own_state_not_the_flags() {
        let asked = RoomOptions {
            item_cheat: false,
            ..Default::default()
        };
        let restored = RoomOptions {
            item_cheat: true,
            ..Default::default()
        };
        let lines = overruled_options(&["--no-item-cheat"], &asked, &restored);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("asked for disabled") && lines[0].contains("says enabled"),
            "{}",
            lines[0]
        );
    }
}
