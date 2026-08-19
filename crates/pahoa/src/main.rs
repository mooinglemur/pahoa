//! pahoa — an Archipelago multiworld server.
//!
//! Argument parsing, wiring, and three subcommands: `serve`, `inspect`, and a
//! `selftest` that exists because a static binary in a `scratch` image has no
//! test runner and "it linked" is not the same as "it computes the right
//! answers". Everything with behavior lives in the crates below this one.

mod cli;
mod inspect;
mod secrets;
mod serve;

use cli::{Opt, flag, value};
use pahoa_proto::Permission;
use pahoa_room::RoomOptions;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use tracing::level_filters::LevelFilter;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
pahoa — Archipelago multiworld server

USAGE:
    pahoa serve <file.archipelago> [options]
                                     Host a multiworld
    pahoa inspect <file.archipelago> [--snapshot <datapackage.json>]
                                     Summarize a multidata file
    pahoa selftest                   Verify the build against known-answer tests
    pahoa --version

SERVE OPTIONS
    --bind <addr>            Listen address (default 0.0.0.0)
    --port <n>               Listen port (default 38281)
    --filtered-port <n>      A second port serving the scoped feed: a client
                             connecting here receives only what concerns its own
                             slot. Needs no client support — the port is the
                             interface.
    --snapshot <file.json>   Data package snapshot, from export-datapackage.py
    --save-dir <dir>         Where the room persists itself
    --save-interval <secs>   Save cadence (default 60)
    --outbound-budget <MiB>  Cap on queued outbound data across all clients.
                             Defaults to 288 KiB per slot, floored at 64 MiB —
                             a 2000-slot room gets 562 MiB, a small one 64.
    --log-level <level>      trace, debug, info, warn, error (default info).
                             Logs go to stderr; stdout carries only the one
                             startup line.
    --tls-cert <file.pem>    Certificate chain. Terminates TLS on the room port,
                             which then serves wss:// and https://. Reloaded in
                             place when the file changes, so a renewal needs no
                             restart.
    --tls-key <file.pem>     Private key for --tls-cert. Both or neither.
    --allow-plaintext        Keep answering ws:// after a certificate is
                             configured. Off by default: it puts the admin
                             token's traffic in the clear.
    --open-tracker           Serve /api/tracker without the admin token even
                             when one is configured. Without a token the tracker
                             is open anyway; with one it is gated, so that a
                             port scan cannot read slot names out of a room.

ROOM OPTIONS
    --password <pw>              Required from every client on connect
    --server-password <pw>       Enables !admin login; unset refuses it outright
    --hint-cost <percent>        Hint price, as a percentage of a slot's own
                                 location count (default 10; 0 makes hints free)
    --location-check-points <n>  Points earned per check (default 1)
    --release-mode <mode>        auto, enabled, disabled, goal, auto-enabled
                                 (default auto)
    --collect-mode <mode>        as --release-mode (default auto)
    --remaining-mode <mode>      enabled, disabled, goal (default goal)
    --countdown-mode <mode>      enabled, disabled, auto (default enabled)
    --no-item-cheat              Refuse !getitem
    --compatibility <0|1|2>      0 exact client version match, 1 strict,
                                 2 permissive (default 2)
    --use-embedded-options       Take every ROOM OPTION from the seed's own
                                 server_options instead, overriding the flags
                                 above where the seed sets them

The reference server's underscored spellings (--hint_cost, --release_mode,
--disable_item_cheat, --host, …) are accepted as aliases.

The data package snapshot is produced by tools/export-datapackage.py. Without
it, games are resolved from the seed's embedded package alone, which covers
names and ids but never hint blacklists.

--save-dir is an ordinary directory; one room per directory, claimed with an
exclusive lock for as long as the process runs. Without it the room keeps
nothing across a restart. --save-interval (default 60) is how much play the
room may lose on an unclean stop.
";

const SERVE_OPTS: &[Opt] = &[
    flag("--help", &["-h"]),
    value("--bind", &["--host"]),
    value("--port", &[]),
    value("--snapshot", &[]),
    value("--save-dir", &[]),
    value("--save-interval", &[]),
    value("--outbound-budget", &[]),
    value("--log-level", &["--loglevel"]),
    value("--filtered-port", &["--filtered_port"]),
    value("--tls-cert", &["--tls_cert"]),
    value("--tls-key", &["--tls_key"]),
    flag("--allow-plaintext", &["--allow_plaintext"]),
    flag("--open-tracker", &["--open_tracker"]),
    value("--password", &[]),
    value("--server-password", &["--server_password"]),
    value("--hint-cost", &["--hint_cost"]),
    value("--location-check-points", &["--location_check_points"]),
    value("--release-mode", &["--release_mode"]),
    value("--collect-mode", &["--collect_mode"]),
    value("--remaining-mode", &["--remaining_mode"]),
    value("--countdown-mode", &["--countdown_mode"]),
    flag("--no-item-cheat", &["--disable_item_cheat"]),
    value("--compatibility", &[]),
    flag("--use-embedded-options", &["--use_embedded_options"]),
];

const INSPECT_OPTS: &[Opt] = &[flag("--help", &["-h"]), value("--snapshot", &[])];

/// `!release` and `!collect` test their mode with `"enabled" in mode`, so every
/// spelling means something for them. `!remaining` and `!countdown` compare for
/// **equality**, so a value like `auto-enabled` would match no branch and sit
/// there doing nothing — which is why their choices are narrower here, as they
/// are in the reference's own argparse (`MultiServer.py:2618-2643`).
const RELEASE_MODES: &[Permission] = &[
    Permission::Auto,
    Permission::Enabled,
    Permission::Disabled,
    Permission::Goal,
    Permission::AutoEnabled,
];
const REMAINING_MODES: &[Permission] =
    &[Permission::Enabled, Permission::Disabled, Permission::Goal];
const COUNTDOWN_MODES: &[Permission] =
    &[Permission::Enabled, Permission::Disabled, Permission::Auto];

/// What `--log-level` advertises, and what its error message lists.
const LOG_LEVELS: &[(&str, LevelFilter)] = &[
    ("trace", LevelFilter::TRACE),
    ("debug", LevelFilter::DEBUG),
    ("info", LevelFilter::INFO),
    ("warn", LevelFilter::WARN),
    ("error", LevelFilter::ERROR),
];

/// Also accepted, and not advertised — the same bargain [`Opt::aliases`] makes.
/// These are Python `logging`'s spellings, which is what anyone arriving from
/// the reference server's `--loglevel` will type.
const LOG_LEVEL_ALIASES: &[(&str, LevelFilter)] = &[
    ("warning", LevelFilter::WARN),
    ("critical", LevelFilter::ERROR),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };

    let result = match cmd.as_str() {
        "--version" | "-V" => {
            println!("pahoa {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        "--help" | "-h" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        "serve" => serve_command(rest),
        "inspect" => inspect_command(rest),
        "selftest" => selftest(),
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };

    report(result)
}

fn serve_command(argv: &[String]) -> Result<(), String> {
    let args = cli::parse(argv, SERVE_OPTS)?;
    if args.is_set("--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let multidata = one_path(&args, "serve")?;

    // Resolved before anything else can fail on a typo, so a room with a
    // contradictory password configuration says so rather than starting.
    let secrets = secrets::resolve(secrets::FromArgv {
        password: args.get("--password"),
        server_password: args.get("--server-password"),
    })?;

    // Without the secrets: `serve::run` applies those over the top, so that the
    // seed's embedded options land between argv and the environment.
    let mut options = RoomOptions::default();
    if let Some(v) = args.number::<u32>("--hint-cost")? {
        options.hint_cost = v;
    }
    if let Some(v) = args.number::<u32>("--location-check-points")? {
        options.location_check_points = v;
    }
    if let Some(v) = args.get("--release-mode") {
        options.release_mode = mode("--release-mode", v, RELEASE_MODES)?;
    }
    if let Some(v) = args.get("--collect-mode") {
        options.collect_mode = mode("--collect-mode", v, RELEASE_MODES)?;
    }
    if let Some(v) = args.get("--remaining-mode") {
        options.remaining_mode = mode("--remaining-mode", v, REMAINING_MODES)?;
    }
    if let Some(v) = args.get("--countdown-mode") {
        options.countdown_mode = mode("--countdown-mode", v, COUNTDOWN_MODES)?;
    }
    if args.is_set("--no-item-cheat") {
        options.item_cheat = false;
    }
    // Parsed wide and range-checked, so that `--compatibility 7` is told what
    // the choices are rather than "expected a number".
    if let Some(v) = args.number::<i64>("--compatibility")? {
        if !(0..=2i64).contains(&v) {
            return Err(format!("--compatibility: expected 0, 1 or 2, got {v}"));
        }
        options.compatibility = v as u8;
    }

    let save_interval = match args.number::<u64>("--save-interval")? {
        // Zero would spin the actor on a timer that fires continuously, so it
        // is an error rather than a clever way to ask for constant saving.
        Some(0) => return Err("--save-interval: must be at least 1 second".to_string()),
        Some(v) => Duration::from_secs(v),
        None => Duration::from_secs(60),
    };

    let outbound_budget_bytes = match args.number::<usize>("--outbound-budget")? {
        Some(0) => return Err("--outbound-budget: must be at least 1 MiB".to_string()),
        Some(mib) => Some(mib * 1024 * 1024),
        None => None,
    };

    let log_level = match args.get("--log-level") {
        Some(v) => log_level(v)?,
        None => LevelFilter::INFO,
    };

    // The parser has no notion of options that require each other, so the pair
    // is checked by hand. Half a pair is always a mistake, and the failure it
    // would otherwise produce is a room that quietly serves plaintext.
    let tls = match (args.get("--tls-cert"), args.get("--tls-key")) {
        (Some(cert), Some(key)) => Some(pahoa_net::TlsPaths {
            cert: Path::new(cert).to_path_buf(),
            key: Path::new(key).to_path_buf(),
        }),
        (None, None) => None,
        (Some(_), None) => return Err("--tls-cert needs --tls-key".to_string()),
        (None, Some(_)) => return Err("--tls-key needs --tls-cert".to_string()),
    };
    let filtered_port = args.number::<u16>("--filtered-port")?;
    if filtered_port == Some(args.number("--port")?.unwrap_or(38281)) {
        return Err("--filtered-port must differ from --port".to_string());
    }

    let allow_plaintext = args.is_set("--allow-plaintext");
    if allow_plaintext && tls.is_none() {
        return Err(
            "--allow-plaintext only means something with --tls-cert; without one \
             this room already serves plaintext and nothing else"
                .to_string(),
        );
    }

    serve::run(serve::ServeArgs {
        multidata: Path::new(multidata),
        snapshot: args.get("--snapshot").map(Path::new),
        outbound_budget_bytes,
        log_level,
        secrets,
        tls,
        allow_plaintext,
        open_tracker: args.is_set("--open-tracker"),
        filtered_port,
        port: args.number("--port")?.unwrap_or(38281),
        bind: args.get("--bind").unwrap_or("0.0.0.0").to_string(),
        save_dir: args.get("--save-dir").map(Path::new),
        save_interval,
        options,
        use_embedded_options: args.is_set("--use-embedded-options"),
    })
}

fn inspect_command(argv: &[String]) -> Result<(), String> {
    let args = cli::parse(argv, INSPECT_OPTS)?;
    if args.is_set("--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let path = one_path(&args, "inspect")?;
    inspect::run(Path::new(path), args.get("--snapshot").map(Path::new))
}

/// Exactly one multidata path.
///
/// A second one is an error rather than a file quietly ignored: the shape it
/// arrives in is a shell glob that matched more seeds than the operator meant,
/// and serving the first of them silently is the wrong answer.
fn one_path<'a>(args: &'a cli::Parsed, cmd: &str) -> Result<&'a str, String> {
    match args.positional.as_slice() {
        [only] => Ok(only.as_str()),
        [] => Err(format!("{cmd} needs a multidata path")),
        many => Err(format!(
            "{cmd} takes one multidata path, got {}: {}",
            many.len(),
            many.join(" ")
        )),
    }
}

/// Strict mode parsing, for the command line only.
///
/// [`Permission::from_text`] is deliberately lenient — it reproduces the
/// reference's substring test, where an unrecognized word quietly becomes
/// `disabled`. That is right for a multidata field and wrong for a flag: an
/// operator who types `--release-mode enable` should be told, not handed a room
/// where nobody can release and no message saying so.
fn mode(name: &str, text: &str, choices: &[Permission]) -> Result<Permission, String> {
    choices
        .iter()
        .copied()
        .find(|p| p.as_text() == text)
        .ok_or_else(|| {
            let names: Vec<&str> = choices.iter().map(|p| p.as_text()).collect();
            format!("{name}: expected one of {}, got {text:?}", names.join(" "))
        })
}

/// Strict level parsing, on the same terms as [`mode`]: a typo is told what the
/// choices are rather than silently leaving the room at its default verbosity.
fn log_level(text: &str) -> Result<LevelFilter, String> {
    LOG_LEVELS
        .iter()
        .chain(LOG_LEVEL_ALIASES)
        .find(|(name, _)| *name == text)
        .map(|(_, level)| *level)
        .ok_or_else(|| {
            let names: Vec<&str> = LOG_LEVELS.iter().map(|(n, _)| *n).collect();
            format!(
                "--log-level: expected one of {}, got {text:?}",
                names.join(" ")
            )
        })
}

fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pahoa: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Known-answer checks over both foundation crates.
///
/// This exists so a built artifact can prove itself in the target environment —
/// a static musl binary in a scratch image has no test runner, and "it linked"
/// is not the same as "it computes the right answers".
fn selftest() -> Result<(), String> {
    use pahoa_pickle::{Allowlist, PyObj, from_slice};
    use pahoa_pyrandom::PyRandom;

    // Pickle: a NEWOBJ-constructed namedtuple with a nested REDUCE enum, which
    // is the shape multidata actually uses for NetworkSlot.
    let stream = b"\x80\x04\x8c\x08NetUtils\x8c\x0bNetworkSlot\x93(\x8c\x01n\x8c\x01gK\x01)t\x81.";
    let v = from_slice(stream, &Allowlist::archipelago()).map_err(|e| format!("pickle: {e}"))?;
    let args = v
        .as_instance_of("NetUtils", "NetworkSlot")
        .ok_or("pickle: did not decode a NetworkSlot")?;
    if args.len() != 4 || args[0].as_str() != Some("n") {
        return Err(format!("pickle: unexpected NetworkSlot {args:?}"));
    }

    // Pickle: the allowlist must refuse the classic RCE gadget.
    let gadget = b"\x80\x04\x8c\x02os\x8c\x06system\x93\x8c\x02ls\x85R.";
    if from_slice(gadget, &Allowlist::archipelago()).is_ok() {
        return Err("pickle: allowlist failed to refuse os.system".into());
    }

    // Bignum passthrough. This exact value appears in real multidata as
    // slot_data[..]["seed_name"], and exceeds u64 — LONG1 with 9 little-endian
    // bytes.
    let big = from_slice(
        b"\x80\x04\x8a\x09\x2d\xe2\x10\x8f\xa3\x8f\xbe\x16\x03.",
        &Allowlist::archipelago(),
    )
    .map_err(|e| format!("pickle: {e}"))?;
    match &big {
        PyObj::Big(b) if b.to_string() == "56979137468180783661" => {}
        other => return Err(format!("pickle: bignum decoded as {other:?}")),
    }

    // PRNG: first draws for a known seed, matching CPython 3.13.
    let mut r = PyRandom::seed_str("TestSeed12345");
    let got: Vec<u64> = (0..5).map(|_| r.getrandbits_u64(32)).collect();
    let want = [
        3640632534u64,
        2509922890,
        3181089231,
        2733124716,
        4261827827,
    ];
    if got != want {
        return Err(format!("pyrandom: got {got:?}, want {want:?}"));
    }

    println!("selftest: ok (pickle, allowlist, bignum, pyrandom)");
    Ok(())
}
