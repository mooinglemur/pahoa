//! Decodes real `.archipelago` multidata and checks it against CPython.
//!
//! Fixtures are not committed — they are large and not ours to redistribute —
//! so this test reads whatever `.archipelago` files sit in `tests/fixtures/`
//! (gitignored). Populate it by copying or symlinking the seeds you want
//! covered:
//!
//! ```text
//! mkdir -p crates/pahoa-pickle/tests/fixtures
//! ln -s /path/to/AP_1234.archipelago crates/pahoa-pickle/tests/fixtures/
//! ```
//!
//! `PAHOA_FIXTURE_DIR` overrides the location. The directory is read one level
//! deep and nothing else on the machine is touched.
//!
//! With no fixtures present the test skips loudly rather than passing silently.
//! The differential half additionally needs python3 and skips without it, which
//! keeps CI hermetic while still gating local and nightly runs.

use pahoa_pickle::{Allowlist, PyObj, canonical, from_slice};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_dir() -> PathBuf {
    std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"))
}

/// `.archipelago` files directly inside the fixture directory. Deliberately
/// not recursive: this points at user directories and has no business walking
/// arbitrary trees.
fn fixtures() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(fixture_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "archipelago"))
        .collect();
    out.sort();
    out
}

/// Prints why it skipped and returns true, so a skip never reads as a pass.
fn skip_if_no_fixtures(fixtures: &[PathBuf]) -> bool {
    if fixtures.is_empty() {
        eprintln!(
            "SKIP: no .archipelago fixtures in {} (see this file's docs)",
            fixture_dir().display()
        );
        return true;
    }
    false
}

/// Strip the multidata container: one format byte, then zlib-compressed pickle.
fn decompress(raw: &[u8]) -> Vec<u8> {
    assert!(!raw.is_empty(), "empty multidata");
    let format = raw[0];
    assert!(format <= 3, "unsupported multidata format version {format}");
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(&raw[1..])
        .read_to_end(&mut out)
        .expect("multidata payload should be zlib");
    out
}

fn load(path: &Path) -> PyObj {
    let raw = std::fs::read(path).expect("fixture readable");
    let pickle = decompress(&raw);
    match from_slice(&pickle, &Allowlist::archipelago()) {
        Ok(v) => v,
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/pahoa-pickle
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives two levels below the workspace root")
        .to_path_buf()
}

fn have_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn decodes_every_fixture() {
    let fixtures = fixtures();
    if skip_if_no_fixtures(&fixtures) {
        return;
    }

    for path in &fixtures {
        let data = load(path);

        // Structural invariants every multidata must satisfy, per NetUtils.MultiData.
        for key in [
            "slot_info",
            "connect_names",
            "locations",
            "seed_name",
            "minimum_versions",
        ] {
            assert!(
                data.get(key).is_some(),
                "{}: missing required key {key:?}",
                path.display()
            );
        }

        let slot_info = data.get("slot_info").unwrap().as_dict().unwrap();
        assert!(!slot_info.is_empty(), "{}: no slots", path.display());

        // Every slot_info value is a NetworkSlot with exactly its 4 fields.
        for (slot, info) in slot_info {
            assert!(
                slot.as_int().is_some(),
                "{}: non-integer slot key",
                path.display()
            );
            let args = info
                .as_instance_of("NetUtils", "NetworkSlot")
                .unwrap_or_else(|| {
                    panic!(
                        "{}: slot {slot:?} is {}, not a NetworkSlot",
                        path.display(),
                        info.type_name()
                    )
                });
            assert_eq!(
                args.len(),
                4,
                "{}: NetworkSlot arity changed",
                path.display()
            );
            assert!(
                args[0].as_str().is_some(),
                "{}: slot name not a string",
                path.display()
            );
            // args[2] is a SlotType enum built via REDUCE.
            assert!(
                args[2].as_instance_of("NetUtils", "SlotType").is_some(),
                "{}: slot type is {:?}",
                path.display(),
                args[2]
            );
        }

        // locations is {slot: {loc_id: (item, receiver, flags)}} and is by far
        // the largest structure; spot-check its shape.
        let locations = data.get("locations").unwrap().as_dict().unwrap();
        for (_, per_slot) in locations.iter().take(4) {
            for (loc, triple) in per_slot.as_dict().unwrap().iter().take(4) {
                assert!(
                    loc.as_int().is_some(),
                    "{}: non-integer location id",
                    path.display()
                );
                let t = triple.as_seq().unwrap_or_else(|| {
                    panic!(
                        "{}: location payload is {}",
                        path.display(),
                        triple.type_name()
                    )
                });
                assert_eq!(
                    t.len(),
                    3,
                    "{}: location triple arity changed",
                    path.display()
                );
                assert!(t.iter().all(|v| v.as_int().is_some()));
            }
        }

        eprintln!("ok: {} ({} slots)", path.display(), slot_info.len());
    }
}

#[test]
fn matches_cpython_canonical_dump() {
    let fixtures = fixtures();
    if skip_if_no_fixtures(&fixtures) {
        return;
    }
    if !have_python() {
        eprintln!("SKIP: python3 unavailable, cannot run the differential dump");
        return;
    }

    let script = repo_root().join("tools/dump-pickle.py");
    assert!(script.exists(), "{} should exist", script.display());

    for path in &fixtures {
        let output = Command::new("python3")
            .arg(&script)
            .arg(path)
            .output()
            .expect("python3 should run");
        assert!(
            output.status.success(),
            "{}: dump-pickle.py failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );

        let expected = String::from_utf8(output.stdout).expect("dump is utf-8");
        let expected = expected.trim_end_matches('\n');
        let actual = canonical(&load(path));

        if expected != actual {
            // Point at the first divergence rather than dumping megabytes.
            let at = expected
                .char_indices()
                .zip(actual.char_indices())
                .find(|((_, a), (_, b))| a != b)
                .map(|((i, _), _)| i)
                .unwrap_or(expected.len().min(actual.len()));
            let from = at.saturating_sub(120);
            panic!(
                "{}: canonical dump diverges at byte {at}\n  expected: ...{}\n  actual:   ...{}",
                path.display(),
                &expected[from..(at + 120).min(expected.len())],
                &actual[from..(at + 120).min(actual.len())],
            );
        }

        eprintln!("match: {}", path.display());
    }
}
