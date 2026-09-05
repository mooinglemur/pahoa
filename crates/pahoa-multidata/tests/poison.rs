//! A hostile multidata, and the three bounds that refuse it.
//!
//! # Why the file is committed
//!
//! `MultiData::parse` has no encoder beside it — a seed can be read and never
//! written — so a malicious one cannot be synthesized in a test. The only way to
//! keep this refused is to hold the real sample, and that is what
//! `tests/fixtures/poisoned_multidata.archipelago` is: 38,559 bytes, no personal
//! data, and the actual file that crashed an Archipelago host in the wild.
//! `.gitignore` carries a deliberate exception for it.
//!
//! # What it does
//!
//! One slot, one location, and 11,422,785 copies of the integer zero in
//! `precollected_items`. Pure repetition, so it inflates **593:1** where real
//! seeds manage between 2.29:1 and 4.55:1 — 38 KB of file becomes 22 MB of
//! pickle and, before these limits, **1.55 GiB of peak RSS** through this
//! parser. Upstream reportedly pays about a gigabyte per room for it, because
//! `precollected_items` outlives the parse as items handed to a slot at connect.
//!
//! # Three bounds, because they fail differently
//!
//! Each of the tests below removes one attacker option, and none of them
//! subsumes the others:
//!
//! - the **inflate cap** bounds a bomb whose input is small,
//! - the **object budget** bounds what is built from bytes that were already
//!   admitted — the only one that survives a payload compressing legitimately,
//!   since cost is a function of opcode count rather than of input size,
//! - the **start-inventory cap** bounds what the *room* holds afterwards, which
//!   is where this file's real cost lives and which bytes cannot express.

use pahoa_multidata::MultiData;
use std::path::PathBuf;

fn poisoned() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/poisoned_multidata.archipelago");
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "the poisoned fixture is committed and should be present at {}: {e}",
            path.display()
        )
    })
}

/// **The headline: it is refused, and refused cheaply.**
///
/// Before the object budget this parsed successfully and cost 1.55 GiB. The
/// refusal now happens inside the pickle reader, before typing, which is why
/// the error names the object count rather than the field.
#[test]
fn the_poisoned_multidata_is_refused() {
    let err = MultiData::parse(&poisoned())
        .expect_err("a file that costs a gigabyte must not parse successfully");
    let text = err.to_string();
    assert!(
        text.contains("objects") || text.contains("precollected") || text.contains("inflates"),
        "refused for the wrong reason, which means one of the bounds moved: {text}"
    );
}

/// The reader stops building rather than truncating.
///
/// **An error is the right answer, not a truncation.** A parser that silently
/// returned the first four million objects would hand back a multidata missing
/// most of a world, which is worse than refusing the file: the room would come
/// up and be wrong.
#[test]
fn the_object_budget_refuses_rather_than_truncating() {
    let raw = poisoned();
    // Straight to the reader, so this is about the budget and not about
    // anything `MultiData` does with the result.
    let mut pickle = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&raw[1..]), &mut pickle)
        .expect("the fixture decompresses; that is the problem with it");
    assert!(
        pickle.len() > 20_000_000,
        "the fixture should inflate to about 22 MB, got {}",
        pickle.len()
    );

    let err = pahoa_pickle::from_slice(&pickle, &pahoa_pickle::Allowlist::archipelago())
        .expect_err("11.4 million objects is past any real seed");
    assert!(
        err.to_string().contains("objects"),
        "expected the object budget to fire: {err}"
    );
}

/// **The bound a caller cannot supply**, stated as a relationship rather than a
/// number so that moving either constant has to be deliberate.
///
/// A byte cap on the input does not imply an object cap: an integer is two
/// bytes of pickle, so the inflate limit alone still admits tens of millions of
/// objects. If the object budget ever rises past what the inflate cap can
/// produce it has stopped bounding anything.
#[test]
fn the_object_budget_binds_before_the_inflate_cap_could() {
    // A compile-time assertion, so a constant edited past this fails the build
    // rather than a test run somebody might not be watching.
    const {
        assert!(
            pahoa_pickle::MAX_OBJECTS < pahoa_multidata::MAX_PICKLE_BYTES as usize / 2,
            "the inflate cap admits more objects than the object budget allows, \
             so the budget bounds nothing"
        );
    }
}

/// **The limits must clear every real seed, with room to spare.**
///
/// A limit that refuses a legitimate seed is a worse bug than the one it fixes,
/// and it fails quietly — on an organizer's upload rather than here. These are
/// the corpus figures the constants were argued against, kept as a test so that
/// lowering one has to confront them.
#[test]
fn the_limits_clear_the_largest_seed_anyone_has() {
    // Measured across a sixteen-seed corpus: the synthetic 2000-slot seed is
    // the largest, at 7,276,310 bytes of pickle and 2,379,014 opcodes. Opcodes
    // are an upper bound on objects built, since several push nothing.
    const LARGEST_PICKLE: u64 = 7_276_310;
    const LARGEST_OPCODES: usize = 2_379_014;

    const {
        assert!(
            pahoa_multidata::MAX_PICKLE_BYTES >= LARGEST_PICKLE * 4,
            "the inflate cap leaves under 4x headroom over the largest known seed"
        );
        assert!(
            pahoa_pickle::MAX_OBJECTS >= LARGEST_OPCODES * 3 / 2,
            "the object budget leaves under 1.5x headroom over the largest known \
             seed; a slightly bigger sync would be refused"
        );
        // The corpus maximum for start inventory is 2,975 across all slots.
        assert!(
            pahoa_multidata::MAX_PRECOLLECTED_ITEMS >= 2_975 * 10,
            "the start-inventory cap leaves under 10x headroom"
        );
    }
}

// --- each bound on its own -------------------------------------------------
//
// The poisoned fixture exercises the object budget, and only that: it inflates
// to 22 MB, well under the inflate cap, and the budget refuses it long before
// typing reaches `precollected_items`. Layered defenses that are only ever
// tested together are one edit away from being one defense, so the two below
// are driven directly.

use pahoa_pickle::PyObj;

fn s(text: &str) -> PyObj {
    PyObj::Str(text.into())
}

fn dict(pairs: Vec<(PyObj, PyObj)>) -> PyObj {
    PyObj::Dict(pairs)
}

fn version(major: i64, minor: i64, build: i64) -> PyObj {
    PyObj::Tuple(vec![
        PyObj::Int(major),
        PyObj::Int(minor),
        PyObj::Int(build),
    ])
}

/// The smallest tree `from_py` will walk as far as `precollected_items`,
/// granting `granted` items to slot 1.
///
/// Built as `PyObj` rather than as a file because there is no encoder: this is
/// the only way to reach the typing layer with a shape no real generator emits.
fn multidata_granting(granted: usize) -> PyObj {
    // A real `NetUtils.NetworkSlot` instance rather than a dict: the typing
    // layer refuses anything else, which is its own small piece of hardening.
    let slot = PyObj::Instance {
        class: pahoa_pickle::ClassId::new("NetUtils", "NetworkSlot"),
        args: vec![
            s("P"),
            s("G"),
            PyObj::Instance {
                class: pahoa_pickle::ClassId::new("NetUtils", "SlotType"),
                args: vec![PyObj::Int(1)],
            },
            PyObj::Tuple(vec![]),
        ],
    };
    dict(vec![
        (s("seed_name"), s("minimal")),
        (s("version"), version(0, 6, 7)),
        (
            s("minimum_versions"),
            dict(vec![(s("server"), version(0, 1, 6))]),
        ),
        (s("slot_info"), dict(vec![(PyObj::Int(1), slot)])),
        (
            s("connect_names"),
            dict(vec![(
                s("P"),
                PyObj::Tuple(vec![PyObj::Int(0), PyObj::Int(1)]),
            )]),
        ),
        (s("locations"), dict(vec![(PyObj::Int(1), dict(vec![]))])),
        (
            s("precollected_items"),
            dict(vec![(
                PyObj::Int(1),
                PyObj::List(vec![PyObj::Int(0); granted]),
            )]),
        ),
    ])
}

/// **The start-inventory cap, with nothing else in the way.**
///
/// The field the known attack uses, and the one whose cost outlives the parse:
/// these become items handed to a slot at connect and held for the room's
/// lifetime, so bytes are the wrong unit for them and the object budget is the
/// wrong bound. A seed can stay well inside every size limit and still ask a
/// room to carry a hundred million items forever.
#[test]
fn the_start_inventory_cap_refuses_by_name() {
    let ok = MultiData::from_py(&multidata_granting(64))
        .expect("an ordinary start inventory must still load");
    assert_eq!(ok.precollected_items[&1].len(), 64);

    let over = pahoa_multidata::MAX_PRECOLLECTED_ITEMS + 1;
    // Mapped to the message before asserting: the success value here holds a
    // hundred thousand items, and a failing `expect_err` would print all of
    // them.
    let outcome = MultiData::from_py(&multidata_granting(over)).map(|_| ());
    let err = outcome.expect_err("a room should never be asked to load this many");
    assert!(
        err.to_string().contains("precollected"),
        "expected the start-inventory cap to fire, got: {err}"
    );
}

/// **The inflate cap, with nothing else in the way.**
///
/// The poisoned fixture cannot test this — it inflates to 22 MB, comfortably
/// under the limit — so the bomb is built here. It need not be valid pickle:
/// the point is that the refusal happens during decompression, before a single
/// object is constructed.
///
/// zlib reaches about 1032:1, so without this the only ceiling was whatever the
/// caller was willing to hand over: an orchestrator taking 256 MiB uploads was
/// offering 264 GiB of inflate to one request.
#[test]
fn the_inflate_cap_refuses_a_bomb_before_decoding_anything() {
    use std::io::Write;

    let over = pahoa_multidata::MAX_PICKLE_BYTES as usize + 1024;
    let mut file = vec![3u8]; // format byte
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    // Zeros, so this compresses to a few kilobytes — the whole point of a bomb.
    let chunk = vec![0u8; 1 << 20];
    let mut written = 0;
    while written < over {
        let n = chunk.len().min(over - written);
        encoder.write_all(&chunk[..n]).expect("encodes");
        written += n;
    }
    file.extend(encoder.finish().expect("finishes"));

    assert!(
        file.len() < 200_000,
        "the bomb should be tiny on disk; got {} bytes",
        file.len()
    );

    let err = MultiData::parse(&file).expect_err("a 64 MiB inflate must be refused");
    assert!(
        err.to_string().contains("inflates"),
        "expected the inflate cap to fire, got: {err}"
    );
}
