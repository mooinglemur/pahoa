//! `pahoa inspect` must agree with `tools/inspect-multidata.py`, line for line.
//!
//! This is M1's exit gate. The two implementations share no code — the Python
//! side reimplements the merge policy rather than importing Archipelago — so
//! agreement over real seeds exercises slot typing, the location table, hints,
//! version floors and the data-package merge against an independent reading of
//! the format.
//!
//! Fixtures live in `crates/pahoa-pickle/tests/fixtures/` (gitignored; see that
//! crate's `fixtures.rs`). Skips loudly when they or python3 are absent.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives two levels below the workspace root")
        .to_path_buf()
}

fn fixture_dir() -> PathBuf {
    std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("crates/pahoa-pickle/tests/fixtures"))
}

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

fn have_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn inspect_matches_the_python_reference() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!(
            "SKIP: no .archipelago fixtures in {} (see crates/pahoa-pickle/tests/fixtures.rs)",
            fixture_dir().display()
        );
        return;
    }
    if !have_python() {
        eprintln!("SKIP: python3 unavailable, cannot run the differential comparison");
        return;
    }

    let script = workspace_root().join("tools/inspect-multidata.py");
    assert!(script.exists(), "{} should exist", script.display());

    for path in &fixtures {
        let ours = Command::new(env!("CARGO_BIN_EXE_pahoa"))
            .args(["inspect".as_ref(), path.as_os_str()])
            .output()
            .expect("pahoa should run");
        assert!(
            ours.status.success(),
            "{}: pahoa inspect failed: {}",
            path.display(),
            String::from_utf8_lossy(&ours.stderr)
        );

        let theirs = Command::new("python3")
            .arg(&script)
            .arg(path)
            .output()
            .expect("python3 should run");
        assert!(
            theirs.status.success(),
            "{}: reference script failed: {}",
            path.display(),
            String::from_utf8_lossy(&theirs.stderr)
        );

        let ours = String::from_utf8(ours.stdout).expect("utf-8");
        let theirs = String::from_utf8(theirs.stdout).expect("utf-8");

        // Compare line by line so a failure names the field, not a byte offset.
        let a: Vec<&str> = theirs.lines().collect();
        let b: Vec<&str> = ours.lines().collect();
        for (i, (want, got)) in a.iter().zip(&b).enumerate() {
            assert_eq!(
                want,
                got,
                "{}: line {} differs\n  python: {want}\n  pahoa:  {got}",
                path.display(),
                i + 1
            );
        }
        assert_eq!(
            a.len(),
            b.len(),
            "{}: python produced {} lines, pahoa produced {}",
            path.display(),
            a.len(),
            b.len()
        );

        eprintln!("match: {} ({} lines)", path.display(), b.len());
    }
}
