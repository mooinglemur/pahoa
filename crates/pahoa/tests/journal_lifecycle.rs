//! The `started`/`stopped` pair, against the real binary.
//!
//! **This has to drive the process, because what it asserts is a fact about
//! process lifetime rather than about any function.** The closing record is
//! written after the actor has stopped, by a handle cloned before the actor
//! took ownership, and it has to reach the disk before the writer thread is
//! joined. Every one of those is an ordering between a signal handler, a
//! runtime shutdown, a channel closing and a thread join — and a unit test of
//! `JournalEvent::stopped` would assert the shape of a value while proving
//! nothing about whether it is ever written.
//!
//! The pair is also what a reader uses to tell one incarnation from the next,
//! so a `stopped` that silently stopped being written would not break anything
//! here; it would make every clean shutdown indistinguishable from a crash, in
//! a file somebody reads months later.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pahoa-lifecycle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Start a room with a journal and wait until it is actually serving.
///
/// Waiting for the announcement rather than sleeping: a debug build parsing a
/// 96-slot seed is not fast, and signaling a room that has not finished its
/// restore would test the wrong path entirely.
fn serving_room(dir: &Path) -> Child {
    serving_room_with_admin(dir, None).0
}

/// As above, and with an admin token it also returns the address the room bound.
///
/// The port is ephemeral and read back off the `serving` event rather than
/// fixed, so concurrent tests and a busy machine cannot collide.
fn serving_room_with_admin(dir: &Path, token: Option<&str>) -> (Child, String) {
    let seed = fixture().expect("caller checked");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pahoa"));
    command
        .arg("serve")
        .arg(&seed)
        .args(["--port", "0"])
        .arg("--save-dir")
        .arg(dir)
        .arg("--journal")
        .args(["--log-format", "json"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env("PAHOA_ADMIN_TOKEN", token);
    }
    let mut child = command.spawn().expect("the binary should start");

    // **Drained to EOF on its own thread, not read until the announcement and
    // then dropped.** Dropping the reader closes the pipe, and every line the
    // room logs from then on — including the whole shutdown sequence — fails to
    // write. That cost an afternoon: the room appeared to die between the
    // signal and its closing record, and the same run by hand was perfect.
    let stderr = child.stderr.take().expect("piped");
    let (announced, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("\"serving\"") {
                let addr = line
                    .split("\"addr\":\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or_default()
                    .to_string();
                let _ = announced.send(addr);
            }
        }
    });

    // Waiting for the announcement rather than sleeping: signaling a room that
    // has not finished its restore would test a different path entirely.
    match ready.recv_timeout(Duration::from_secs(30)) {
        Ok(addr) => (child, addr),
        Err(_) => {
            let _ = child.kill();
            panic!("the room never announced itself");
        }
    }
}

/// `POST /admin/v1/shutdown`, by hand rather than through a client crate.
fn request_shutdown(addr: &str, token: &str) -> String {
    use std::io::{Read, Write};
    let mut socket = std::net::TcpStream::connect(addr).expect("the room is listening");
    write!(
        socket,
        "POST /admin/v1/shutdown HTTP/1.1\r\nHost: {addr}\r\n\
         Authorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .expect("request writes");
    let mut reply = String::new();
    let _ = socket.read_to_string(&mut reply);
    reply.lines().next().unwrap_or_default().to_string()
}

/// Send SIGTERM repeatedly across a window, to land one wherever the shutdown
/// happens to be.
fn hammer_sigterm(pid: u32, times: usize) {
    for _ in 0..times {
        let sent = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        // A failure means the process is already gone, which is the outcome
        // being waited for rather than an error.
        if !sent.map(|s| s.success()).unwrap_or(false) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn records(dir: &Path) -> Vec<serde_json::Value> {
    let path = dir.join("history.jsonl");
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no journal at {}: {e}", path.display()));
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("unparseable journal line ({e}): {line}"))
        })
        .collect()
}

fn wait_for_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("the room did not exit");
}

/// **SIGTERM is the orchestrated path**, and the one that has to work: a
/// Kubernetes drain sends it, and a room that ignored it would be killed
/// outright a grace period later with its whole tail unwritten.
#[test]
fn a_terminated_room_records_that_it_started_and_why_it_stopped() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let dir = temp_dir("sigterm");
    let mut child = serving_room(&dir);

    // SIGTERM by hand: `Child::kill` sends SIGKILL, which is the case that
    // deliberately writes nothing.
    let signaled = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(signaled.success(), "could not signal the room");
    wait_for_exit(&mut child);

    let records = records(&dir);
    let started: Vec<_> = records.iter().filter(|r| r["type"] == "started").collect();
    let stopped: Vec<_> = records.iter().filter(|r| r["type"] == "stopped").collect();

    assert_eq!(started.len(), 1, "one start per incarnation: {records:?}");
    assert_eq!(
        stopped.len(),
        1,
        "a cleanly terminated room left no closing record, so it is \
         indistinguishable from a crash: {records:?}"
    );

    assert_eq!(stopped[0]["reason"], "SIGTERM");
    for record in [started[0], stopped[0]] {
        assert_eq!(record["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(record["build_rev"], env!("PAHOA_BUILD_REV"));
        assert!(
            record["at"].as_f64().is_some_and(|at| at > 1_700_000_000.0),
            "timestamp should be a unix time: {record}"
        );
    }

    // `started` first, and the options that follow it belong to that build.
    assert_eq!(records[0]["type"], "started");
    assert_eq!(
        records.last().expect("non-empty")["type"],
        "stopped",
        "the closing record must sit below the last thing the room did"
    );
}

/// **A kill writes nothing, and the reader is meant to notice.**
///
/// This is the case the pair exists for. Nothing can write a closing record for
/// a process that is already gone, so the absence is the signal — and a second
/// run appending to the same file leaves `started, started` adjacent, which is
/// exactly how a reader spots the incarnation that died.
///
/// Writing the record optimistically at startup would have been the obvious
/// alternative and would say the opposite of the truth in this case, which is
/// the only case worth detecting.
#[test]
fn a_killed_room_leaves_no_closing_record_so_a_crash_is_visible() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let dir = temp_dir("sigkill");

    let mut child = serving_room(&dir);
    let _ = child.kill(); // SIGKILL: no handler, no unwinding, no final write.
    wait_for_exit(&mut child);

    // A second incarnation in the same directory, stopped cleanly this time.
    let mut child = serving_room(&dir);
    let signaled = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(signaled.success());
    wait_for_exit(&mut child);

    let kinds: Vec<String> = records(&dir)
        .iter()
        .filter_map(|r| r["type"].as_str())
        .filter(|t| *t == "started" || *t == "stopped")
        .map(str::to_string)
        .collect();

    assert_eq!(
        kinds,
        ["started", "started", "stopped"],
        "the killed incarnation should show as a start with no stop after it"
    );
}

/// **A SIGTERM arriving after an admin shutdown must not cut the graceful one
/// short.**
///
/// This is the ordinary Kubernetes sequence, not an exotic race: an orchestrator
/// asks the room to stop through the API and the kubelet then terminates the pod
/// anyway, so the two arrive within milliseconds of each other and the signal
/// lands in the middle of the quiesce — after the actor has been told to stop,
/// while the final save is on a blocking thread, or during the linger that lets
/// close frames reach the wire.
///
/// The graceful path survives it because `shutdown_signal()` has already
/// installed a SIGTERM handler by the time either branch of the `select!`
/// resolves, so later signals are caught and have nowhere to go rather than
/// taking the process's default disposition. **Nothing in the source says that
/// out loud**, which is exactly why it is worth a test: a refactor that made the
/// handler conditional, lazy, or scoped to the branch that lost would restore
/// the default disposition and kill the room mid-save, with the failure showing
/// up as an occasional lost save on somebody's cluster.
#[test]
fn a_sigterm_during_an_admin_shutdown_does_not_abort_the_quiesce() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let dir = temp_dir("shutdown-race");
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123";
    let (mut child, addr) = serving_room_with_admin(&dir, Some(TOKEN));

    // Wait for the 202 first, so the admin branch has definitively won the
    // select. Signalling before that would be a different test — one where
    // SIGTERM wins and the graceful path never involves the API at all.
    let status = request_shutdown(&addr, TOKEN);
    assert!(
        status.contains("202"),
        "shutdown was not accepted: {status}"
    );

    // Across the whole window: actor stop, final save, and the 250ms linger.
    hammer_sigterm(child.id(), 20);
    let code = child.wait().expect("the room exits");

    assert!(
        code.success(),
        "the room was killed by a signal instead of finishing its shutdown: {code}"
    );

    // The two things the quiesce exists to produce. Either missing means a
    // signal cut it short, which is invisible from the exit status alone once
    // the process has been replaced by a fresh pod.
    let save = dir.join("room.save");
    assert!(
        save.exists() && save.metadata().is_ok_and(|m| m.len() > 0),
        "the final save never reached disk"
    );

    let stopped: Vec<_> = records(&dir)
        .into_iter()
        .filter(|r| r["type"] == "stopped")
        .collect();
    assert_eq!(
        stopped.len(),
        1,
        "expected exactly one closing record; a second would mean the shutdown \
         path ran twice: {stopped:?}"
    );
    assert_eq!(
        stopped[0]["reason"], "admin request",
        "the first cause should win — the signals arrived after the room had \
         already decided why it was stopping"
    );
}

/// The same collision the other way round, which is the one an operator hits by
/// hand: the signal wins and the API request lands against a room that is
/// already going away.
///
/// It must not produce a second closing record, a second save, or a hang. The
/// request itself may be refused or never answered — the listener is aborted
/// first, deliberately — and that is fine; what matters is the room's own exit.
#[test]
fn an_admin_shutdown_racing_a_sigterm_the_other_way_is_also_clean() {
    if fixture().is_none() {
        eprintln!("SKIP: fixture {FIXTURE} not present");
        return;
    }
    let dir = temp_dir("shutdown-race-rev");
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123";
    let (mut child, addr) = serving_room_with_admin(&dir, Some(TOKEN));

    let pid = child.id();
    let asker = std::thread::spawn(move || {
        // Deliberately unchecked: a room already tearing down may refuse the
        // connection, and that is a correct outcome rather than a failure.
        let _ = request_shutdown(&addr, TOKEN);
    });
    hammer_sigterm(pid, 20);
    let code = child.wait().expect("the room exits");
    let _ = asker.join();

    assert!(code.success(), "{code}");
    let stopped: Vec<_> = records(&dir)
        .into_iter()
        .filter(|r| r["type"] == "stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "{stopped:?}");
}
