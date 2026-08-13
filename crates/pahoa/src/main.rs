//! pahoa — an Archipelago multiworld server.
//!
//! At M1 this is a thin shell over the foundation crates: enough to prove the
//! static build works end to end and to inspect a multidata by hand. The
//! server itself arrives at M4.

mod inspect;
mod serve;

use std::path::Path;
use std::process::ExitCode;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
pahoa — Archipelago multiworld server

USAGE:
    pahoa inspect <file.archipelago> [--snapshot <datapackage.json>]
                                       Summarize a multidata file
    pahoa serve <file.archipelago> [--snapshot <datapackage.json>]
                [--port <n>] [--bind <addr>] [--password <pw>]
                [--save-dir <dir>] [--save-interval <seconds>]
                                       Host a multiworld
    pahoa selftest                     Verify the build against known-answer tests
    pahoa --version

The data package snapshot is produced by tools/export-datapackage.py. Without
it, games are resolved from the seed's embedded package alone, which covers
names and ids but never hint blacklists.

--save-dir is an ordinary directory; one room per directory, claimed with an
exclusive lock for as long as the process runs. Without it the room keeps
nothing across a restart. --save-interval (default 60) is how much play the
room may lose on an unclean stop.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    let result = match cmd {
        Some("--version" | "-V") => {
            println!("pahoa {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") | None => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("serve") => match args.get(1) {
            Some(path) => {
                let opt = |name: &str| {
                    args.iter()
                        .position(|a| a == name)
                        .and_then(|i| args.get(i + 1))
                        .cloned()
                };
                let snapshot = opt("--snapshot");
                let save_dir = opt("--save-dir");
                serve::run(serve::ServeArgs {
                    multidata: Path::new(path),
                    snapshot: snapshot.as_deref().map(Path::new),
                    port: match opt("--port") {
                        Some(p) => match p.parse() {
                            Ok(v) => v,
                            Err(_) => return report(Err(format!("bad --port {p:?}"))),
                        },
                        None => 38281,
                    },
                    bind: opt("--bind").unwrap_or_else(|| "0.0.0.0".to_string()),
                    password: opt("--password"),
                    save_dir: save_dir.as_deref().map(Path::new),
                    save_interval: match opt("--save-interval") {
                        Some(s) => match s.parse::<u64>() {
                            // Zero would spin the actor on a timer that fires
                            // continuously, so it is an error rather than a
                            // clever way to ask for constant saving.
                            Ok(v) if v > 0 => std::time::Duration::from_secs(v),
                            _ => return report(Err(format!("bad --save-interval {s:?}"))),
                        },
                        None => std::time::Duration::from_secs(60),
                    },
                })
            }
            None => Err("serve needs a multidata path".to_string()),
        },
        Some("selftest") => selftest(),
        Some("inspect") => match args.get(1) {
            Some(path) => {
                let snapshot = args
                    .iter()
                    .position(|a| a == "--snapshot")
                    .and_then(|i| args.get(i + 1))
                    .map(Path::new);
                inspect::run(Path::new(path), snapshot)
            }
            None => Err("inspect needs a path".to_string()),
        },
        Some(other) => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };

    report(result)
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
