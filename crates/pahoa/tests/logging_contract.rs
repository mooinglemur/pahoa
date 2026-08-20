//! The startup output, as another system depends on it.
//!
//! **An orchestrator decides a room is really serving by matching the `serving`
//! event's `message` and reading `addr` and `seed_name` off it.** That makes a
//! rename, a moved field, or a stray `println!` a failure *in someone else's
//! cluster*, announced by nothing here — the sort of break a unit test cannot
//! see, because it is a fact about the process's streams rather than about any
//! function.
//!
//! So this drives the real binary and reads what actually comes out of it. Both
//! formats are covered, because the contract is different in each: `text` puts
//! the announcement on stdout in a fixed shape, and `json` puts nothing on
//! stdout at all and makes the announcement an event.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FIXTURE: &str = "AP_14318265276849580066.archipelago";

fn fixture() -> Option<PathBuf> {
    let dir = std::env::var_os("PAHOA_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crate is two levels below the workspace root")
                .join("crates/pahoa-pickle/tests/fixtures")
        });
    let path = dir.join(FIXTURE);
    path.exists().then_some(path)
}

/// Run a room until it has announced itself, then stop it and return
/// `(stdout, stderr)`.
///
/// Both streams are drained by their own thread. A room that filled a pipe
/// buffer while this test slept would block on the write and never reach the
/// line being waited for, which would look exactly like the contract being
/// broken.
fn run_until_serving(extra: &[&str]) -> (String, String) {
    let seed = fixture().expect("caller checked");

    let mut child = Command::new(env!("CARGO_BIN_EXE_pahoa"))
        .arg("serve")
        .arg(&seed)
        // Ephemeral, so concurrent tests and a busy CI machine cannot collide.
        .args(["--port", "0"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary should start");

    let out = Arc::new(Mutex::new(String::new()));
    let err = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    for (stream, sink) in [
        (
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Arc::clone(&out),
        ),
        (Box::new(child.stderr.take().unwrap()), Arc::clone(&err)),
    ] {
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let mut buffer = sink.lock().unwrap();
                buffer.push_str(&line);
                buffer.push('\n');
            }
        }));
    }

    // Wait for the announcement rather than sleeping a fixed time: a debug
    // build parsing a 96-slot seed is not fast, and a timeout long enough to
    // always cover it would be paid by every run.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let announced = out.lock().unwrap().contains("pahoa serving")
            || err.lock().unwrap().contains("\"serving\"");
        if announced {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    for reader in readers {
        let _ = reader.join();
    }

    let stdout = out.lock().unwrap().clone();
    let stderr = err.lock().unwrap().clone();
    (stdout, stderr)
}

fn events(stderr: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stderr line is not JSON ({e}): {line}"))
        })
        .collect()
}

/// Constraint 7, as puna reads it.
#[test]
fn json_announces_the_room_as_a_serving_event_and_leaves_stdout_empty() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let (stdout, stderr) = run_until_serving(&["--log-format", "json"]);

    // Puna's own verification job asserts this too. It is here as well because
    // the thing that would break it — someone adding a `println!` — lives on
    // this side, and finding out from another repository's CI is too late.
    assert!(
        stdout.is_empty(),
        "stdout must be silent under --log-format json, got: {stdout}"
    );

    let events = events(&stderr);
    let serving: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["message"] == "serving")
        .collect();
    assert_eq!(
        serving.len(),
        1,
        "expected exactly one `serving` event; two would make anything counting \
         room starts de-duplicate, none would mean puna never sees the room come up"
    );

    // The fields puna reads. Named individually rather than compared as a whole
    // object, so adding a field stays allowed and removing one does not.
    let serving = serving[0];
    for field in [
        "addr",
        "seed_name",
        "slots",
        "locations",
        "outbound_budget_bytes",
        "version",
        "build_rev",
    ] {
        assert!(
            !serving[field].is_null(),
            "`serving` lost its `{field}` field: {serving}"
        );
    }
    assert_eq!(serving["seed_name"], "14318265276849580066");

    // The banner, which carries the build identity and the sizing evidence
    // constraint 5 is about.
    let banner = events
        .iter()
        .find(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("Pahoa-"))
        })
        .expect("no startup banner");
    assert!(
        banner["message"]
            .as_str()
            .is_some_and(|m| m.ends_with(" starting")),
        "{banner}"
    );
    for field in ["argv", "os", "arch", "pid", "worker_threads"] {
        assert!(!banner[field].is_null(), "banner lost `{field}`: {banner}");
    }
}

/// The other half of the same contract: text keeps the stdout line, and does
/// *not* also emit the event. One announcement, in one place, per format.
#[test]
fn text_keeps_the_startup_line_on_stdout_and_does_not_double_announce() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let (stdout, stderr) = run_until_serving(&[]);

    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "stdout carries exactly one line under --log-format text, got: {stdout}"
    );
    // The shape, field by field. This line predates the JSON work and is what a
    // reader configured before it still matches.
    let line = lines[0];
    assert!(line.starts_with("pahoa serving "), "{line}");
    for part in [
        " slots, ",
        " locations, ",
        "seed 14318265276849580066 on ",
        "(outbound budget ",
        " MiB, version ",
    ] {
        assert!(line.contains(part), "startup line lost `{part}`: {line}");
    }

    assert!(
        !stderr.contains("\"serving\""),
        "text mode also emitted the JSON event, so one room start is two records: {stderr}"
    );
}

/// A room that dies must say why *inside* the JSON stream.
///
/// The fatal line is the worst one to lose: a shipper configured to reject
/// non-JSON would drop precisely the cause of death, and a log view that renders
/// fields would show a bare `eprintln!` as an unattributed fragment or not at
/// all. This is the case that motivated `--log-format json` in the first place,
/// arriving as its own exception.
#[test]
fn a_fatal_error_after_logging_starts_is_a_json_event() {
    let child = Command::new(env!("CARGO_BIN_EXE_pahoa"))
        .args(["serve", "/nonexistent.archipelago", "--log-format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary should start");
    let output = child.wait_with_output().expect("it should exit");

    assert!(!output.status.success(), "a missing seed should be fatal");
    assert!(
        output.stdout.is_empty(),
        "stdout must stay silent under json: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let events = events(&stderr);
    let fatal: Vec<&serde_json::Value> = events.iter().filter(|e| e["level"] == "ERROR").collect();
    assert_eq!(fatal.len(), 1, "expected one ERROR event: {stderr}");
    assert!(
        fatal[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("nonexistent.archipelago")),
        "the error does not say what failed: {}",
        fatal[0]
    );
}

/// The other side of that split, which has to stay `eprintln!`.
///
/// A `--log-format` that failed to parse cannot be reported in the format it
/// names, so this one legitimately escapes — and being explicit about *which*
/// failures do is what makes "every line after startup is JSON" a checkable
/// claim rather than an approximate one.
#[test]
fn a_failure_before_logging_starts_still_prints_plainly() {
    let output = Command::new(env!("CARGO_BIN_EXE_pahoa"))
        .args([
            "serve",
            "/nonexistent.archipelago",
            "--log-format",
            "nonsense",
        ])
        .output()
        .expect("the binary should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("pahoa: --log-format"), "{stderr}");
    // And nothing was logged, because there was no subscriber to log through.
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

/// A password must not reach the log through the banner's `argv` field.
#[test]
fn the_banner_does_not_leak_a_password_from_the_command_line() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let (stdout, stderr) = run_until_serving(&[
        "--log-format",
        "json",
        "--password",
        "correct-horse-battery-staple",
    ]);

    assert!(
        !stderr.contains("correct-horse-battery-staple")
            && !stdout.contains("correct-horse-battery-staple"),
        "the password reached the output:\n{stdout}\n{stderr}"
    );
    // And the field is still there and still useful, rather than dropped.
    let events = events(&stderr);
    let banner = events
        .iter()
        .find(|e| {
            e["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("Pahoa-"))
        })
        .expect("no startup banner");
    let argv = banner["argv"].as_str().expect("argv is a string");
    assert!(argv.contains("--password ***"), "{argv}");
}
