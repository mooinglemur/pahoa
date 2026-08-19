//! Stamps the build's source revision into the binary.
//!
//! The startup banner names the exact tree a room was built from, which is the
//! first question anyone asks about a room behaving oddly and the one thing a
//! version number cannot answer — `0.1.0` is every build for months.
//!
//! **The container build has no `.git`.** `.dockerignore` excludes it, and
//! deliberately: it is large, it changes on every commit, and including it
//! would bust the build context cache constantly. So the revision arrives as an
//! environment variable there (`PAHOA_BUILD_REV`, from `CI_COMMIT_SHORT_SHA`),
//! and this only falls back to asking git when building from a working tree.
//! Neither is fatal: a build that can establish neither says `unknown` rather
//! than failing, because a missing revision is a worse startup line and not a
//! broken server.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PAHOA_BUILD_REV");
    // A commit, a checkout or a staged change moves one of these. A change that
    // is merely *written* moves neither, so the `+` can lag until something
    // else forces a rebuild — accepted, because the alternative is re-running
    // `git status` on every build of every crate.
    for path in [".git/HEAD", ".git/index"] {
        if let Some(git) = find_upward(path) {
            println!("cargo:rerun-if-changed={}", git.display());
        }
    }

    let rev = std::env::var("PAHOA_BUILD_REV")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_revision)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=PAHOA_BUILD_REV={rev}");
}

/// The short hash, with `+` appended when the tree it was built from had
/// uncommitted changes.
fn git_revision() -> Option<String> {
    let hash = run(&["rev-parse", "--short", "HEAD"])?;
    // `--porcelain` is the stable, script-facing form; any output at all means
    // something differs from HEAD. Untracked files count, which is intended: a
    // stray source file is exactly the kind of thing that makes a local build
    // not the commit it claims to be.
    let dirty = run(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
    Some(format!("{hash}{}", if dirty { "+" } else { "" }))
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// Locate a path in this crate's directory or any ancestor.
///
/// The crate sits two levels below the workspace root, and `.git` is at the
/// root, so a plain relative path would never resolve.
fn find_upward(relative: &str) -> Option<std::path::PathBuf> {
    let mut dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    loop {
        let candidate = dir.join(relative);
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}
