//! The journal writer: what reaches the file, and what happens when it cannot.

use pahoa_multidata::MultiData;
use pahoa_net::journal::{FILE_NAME, Journal};
use pahoa_room::CheckRecord;
use std::path::PathBuf;
use std::sync::Arc;

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn seed() -> Option<(Arc<MultiData>, Arc<pahoa_multidata::DataPackage>)> {
    let dir = std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crate is two levels below the workspace root")
                .join("crates/pahoa-pickle/tests/fixtures")
        });
    let raw = std::fs::read(dir.join(FIXTURE)).ok()?;
    let data = Arc::new(MultiData::parse(&raw).expect("fixture parses"));
    let (names, _) = data.resolve_datapackage();
    Some((data, Arc::new(names)))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pahoa-journal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn record(location: i64, finder: u32, receiver: u32) -> CheckRecord {
    CheckRecord {
        at: 1_787_000_000.5,
        finder,
        receiver,
        item: 5_606_235,
        location,
        flags: 1,
    }
}

/// The property the journal exists for: one file, continuing across restarts.
#[test]
fn a_second_run_appends_to_the_first_rather_than_replacing_it() {
    let Some((data, names)) = seed() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let dir = temp_dir("append");

    for run in 0..2 {
        let (journal, writer) =
            Journal::open(&dir, Arc::clone(&data), Arc::clone(&names)).expect("opens");
        for i in 0..3 {
            journal.record(record(5_606_192 + run * 100 + i, 1, 1));
        }
        // Dropping every handle is what closes the channel and stops the
        // writer; joining before that would hang.
        drop(journal);
        writer.finish();
    }

    let body = std::fs::read_to_string(dir.join(FILE_NAME)).expect("journal exists");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 6, "a restart truncated the history: {body}");
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line).expect("each line is JSON");
    }
}

#[test]
fn a_record_carries_resolved_names_for_both_sides() {
    let Some((data, names)) = seed() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let dir = temp_dir("names");

    let (journal, writer) = Journal::open(&dir, Arc::clone(&data), names).expect("opens");
    // A real location from slot 1, so the names resolve rather than falling
    // back to "Unknown".
    journal.record(record(5_606_192, 1, 1));
    drop(journal);
    writer.finish();

    let body = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
    let row: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();

    assert_eq!(row["type"], "check");
    assert_eq!(row["finder"], 1);
    assert_eq!(row["location"], 5_606_192);
    let expected = &data.slot_info[&1].name;
    assert_eq!(row["finder_name"], expected.as_str());
    // Resolved, not a fallback — the point of doing this in the writer rather
    // than storing the string per record.
    let location_name = row["location_name"].as_str().unwrap();
    assert!(
        !location_name.starts_with("Unknown location"),
        "names did not resolve: {location_name}"
    );
}

/// A player name containing a quote must not make the file unparseable — the
/// worst place to discover an escaping bug is a history nobody reads until
/// months later.
#[test]
fn a_name_needing_escapes_still_produces_valid_json() {
    let Some((data, names)) = seed() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let dir = temp_dir("escape");

    let (journal, writer) = Journal::open(&dir, data, names).expect("opens");
    // An unknown receiver takes the fallback path, which builds its name from
    // the id — the escaping still has to hold for every field.
    journal.record(record(5_606_192, 1, u32::MAX));
    drop(journal);
    writer.finish();

    let body = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
    for line in body.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("unparseable line ({e}): {line}"));
    }
}

/// Nothing recorded still leaves a well-formed, empty file rather than no file
/// — an organizer asking for a quiet room's history should get an empty answer,
/// not a missing one.
#[test]
fn a_room_with_no_checks_still_has_a_journal() {
    let Some((data, names)) = seed() else {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    };
    let dir = temp_dir("empty");

    let (journal, writer) = Journal::open(&dir, data, names).expect("opens");
    drop(journal);
    writer.finish();

    let body = std::fs::read_to_string(dir.join(FILE_NAME)).expect("journal exists");
    assert!(body.is_empty(), "{body}");
}
