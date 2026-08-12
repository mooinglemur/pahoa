//! pahoa — an Archipelago multiworld server.
//!
//! At M0 this is a thin shell over the two foundation crates: enough to prove
//! the static build works end to end and to inspect a multidata by hand. The
//! server itself arrives at M4.

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
pahoa — Archipelago multiworld server

USAGE:
    pahoa inspect <file.archipelago>   Summarise a multidata file
    pahoa selftest                     Verify the build against known-answer tests
    pahoa --version
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
        Some("selftest") => selftest(),
        Some("inspect") => match args.get(1) {
            Some(path) => inspect(Path::new(path)),
            None => Err("inspect needs a path".to_string()),
        },
        Some(other) => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };

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

fn inspect(path: &Path) -> Result<(), String> {
    use pahoa_pickle::{Allowlist, from_slice};

    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let format = *raw.first().ok_or("empty file")?;
    if format > 3 {
        return Err(format!("unsupported multidata format version {format}"));
    }

    let mut pickle = Vec::new();
    flate2::read::ZlibDecoder::new(&raw[1..])
        .read_to_end(&mut pickle)
        .map_err(|e| format!("zlib: {e}"))?;

    let data = from_slice(&pickle, &Allowlist::archipelago()).map_err(|e| e.to_string())?;

    let seed = data
        .get("seed_name")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    let slots = data
        .get("slot_info")
        .and_then(|v| v.as_dict())
        .map_or(0, <[_]>::len);
    let games = data
        .get("datapackage")
        .and_then(|v| v.as_dict())
        .map_or(0, <[_]>::len);
    let locations: usize = data
        .get("locations")
        .and_then(|v| v.as_dict())
        .map(|d| {
            d.iter()
                .filter_map(|(_, v)| v.as_dict())
                .map(<[_]>::len)
                .sum()
        })
        .unwrap_or(0);

    println!("file:      {}", path.display());
    println!("format:    {format}");
    println!("seed_name: {seed}");
    println!("slots:     {slots}");
    println!("games:     {games}");
    println!("locations: {locations}");
    Ok(())
}
