//! The room's durable history, as JSON lines beside its save.
//!
//! One line per location checked, appended for as long as the room exists and
//! across every restart of it. This is the organizer's record — "when did each
//! check happen" — and it is deliberately a *file in the room's own directory*
//! rather than something recovered from the log stream.
//!
//! ## Why not the log stream
//!
//! The events would reach a log aggregator perfectly well, and for an operator
//! that is the right place. For an organizer it is the wrong one, on three
//! counts that are all about access rather than durability:
//!
//! - **Authorization.** Loki isolates by tenant and has no label-level access
//!   control, so "this organizer may read this room and nothing else" is not a
//!   thing the store enforces. Here it is a file in a directory the orchestrator
//!   already owns exclusively, and the blast radius of a mistake is one room's
//!   own data.
//! - **Lifetime.** Retention is a platform setting. An async room outliving that
//!   window loses the history the organizer wanted, and nobody involved would
//!   notice until they asked for it.
//! - **Identity across restarts.** Pod logs are labeled by pod, and a restarted
//!   room is a new pod. Reassembling one room's history means promoting a stable
//!   label through the shipper. The save directory is the same directory by
//!   definition, so appending to a file in it is continuous for free.
//!
//! ## Why a thread
//!
//! `release_player` feeds *every* location a slot owns through the check path in
//! one burst — 341,851 of them on a 2000-slot room, against a measured 283 ms
//! for the release itself. Formatting and writing that inline would land on the
//! task that owns all room state, and a slow disk would stall the room rather
//! than merely the journal. So the actor pushes `Copy` records into a bounded
//! channel and a thread does the rest: name resolution, JSON, and the write.
//!
//! The channel is bounded and **drops** when full rather than blocking, because
//! the alternative is letting a stalled disk stop a live multiworld. A drop is
//! never silent: the count is written into the journal itself, so a gap in the
//! history says so in the history.

use pahoa_multidata::{DataPackage as NameTables, MultiData};
use pahoa_room::{CheckRecord, JournalEvent};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the journal is called inside the save directory.
pub const FILE_NAME: &str = "history.jsonl";

/// The most records this will ever buffer, for the largest seed anyone has.
///
/// Reached only by a seed that actually has this many locations; see
/// [`capacity_for`].
const MAX_CAPACITY: usize = 1 << 19;

/// Headroom above the check burst, for the events that are not checks.
///
/// Chat, hints, cheats, connects and admin commands all share this channel and
/// none of them arrive in bursts, so this is generous rather than calculated.
const EVENT_HEADROOM: usize = 8192;

/// Records buffered between the actor and the writer thread, for a seed with
/// this many locations.
///
/// # Why this is not a constant any more
///
/// It was `1 << 19`, sized — correctly — to swallow the largest burst a room
/// can produce in one call, since `release_player` feeds every location a slot
/// owns through the check path at once and that is 341,851 records on a
/// 2000-slot seed. What it missed is that **the same number was then reserved
/// for every other room too**, and `std::sync::mpsc::sync_channel` allocates
/// its whole ring up front and writes a stamp into every slot, so all of it is
/// resident from the moment the room starts.
///
/// Measured: a **1-slot, 97-location** room reserved 524,288 slots of 56 bytes
/// — 28 MiB, resident, for a seed whose every location together is 97 records.
/// That was roughly half the process's RSS, and it was the single largest term
/// in a small room's footprint.
///
/// A seed's location count is a genuine hard bound rather than an estimate:
/// `register_location_checks` filters locations already checked, so no location
/// can produce a second `check` record however many times a client sends it,
/// and a whole-room release is therefore the worst case that exists. Sizing to
/// it keeps the original guarantee exactly — the drop path stays reserved for a
/// disk that has genuinely stopped — while a small room stops paying for a
/// burst its seed cannot express.
pub fn capacity_for(locations: usize) -> usize {
    // The floor falls out of the addition — an empty seed still gets
    // `EVENT_HEADROOM` — so there is nothing to clamp against below.
    locations.saturating_add(EVENT_HEADROOM).min(MAX_CAPACITY)
}

/// How often the writer flushes to the OS when records are still arriving.
///
/// The tail of a journal is lost if the process is killed outright, which is
/// the same bargain the save file makes and for the same reason: an `fsync` per
/// check would make a release disk-bound.
const FLUSH_EVERY: usize = 1024;

/// How long a buffered tail may sit unwritten once the room goes quiet.
///
/// **A count alone scales the wrong way for anything reading this file.** A busy
/// room passes [`FLUSH_EVERY`] constantly and is always fresh; a quiet room
/// reaches the disk only on the save tick, which is a `--save-interval` chosen
/// for how much play a crash may lose and has nothing to do with how stale a
/// reader may be. So the room where somebody is watching a feed go by — one
/// check every few seconds — was the room whose file was worst, up to half a
/// minute behind and then arriving in a burst.
///
/// This shortens the durability window rather than widening it: it is
/// `BufWriter::flush` to the OS, not an `fsync`, so it moves a quiet room's tail
/// out of this process's memory and into the page cache, where a kill no longer
/// takes it. Nothing about the per-check cost changes — a release still batches
/// at [`FLUSH_EVERY`] and still never blocks the actor.
const IDLE_FLUSH: std::time::Duration = std::time::Duration::from_secs(1);

/// A handle the transport hands its effect sink.
#[derive(Clone)]
pub struct Journal {
    tx: std::sync::mpsc::SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

enum Message {
    Check(CheckRecord),
    /// Everything that is not a check. Already shaped as the object to write,
    /// because these are rare enough to afford being built where they happen.
    Event(Box<JournalEvent>),
    Flush,
}

impl Journal {
    /// Open `<dir>/history.jsonl` for appending and start its writer.
    ///
    /// Appends rather than truncates: a restarted room continues the record
    /// that the previous incarnation was keeping, which is the whole reason
    /// this lives next to the save rather than in the log stream.
    pub fn open(
        dir: &Path,
        data: Arc<MultiData>,
        names: Arc<NameTables>,
    ) -> std::io::Result<(Self, JournalWriter)> {
        let path = dir.join(FILE_NAME);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        // From the seed rather than a constant: the whole ring is allocated and
        // stamped up front, so a room pays for this at startup whether or not
        // it ever queues anything. See `capacity_for`.
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity_for(data.locations.len()));
        let dropped = Arc::new(AtomicU64::new(0));

        let handle = std::thread::Builder::new()
            .name("pahoa-journal".to_string())
            .spawn({
                let dropped = Arc::clone(&dropped);
                move || run(file, rx, data, names, dropped)
            })?;

        Ok((
            Self {
                tx,
                dropped: Arc::clone(&dropped),
            },
            JournalWriter {
                handle: Some(handle),
                path,
            },
        ))
    }

    /// Queue one check. Never blocks.
    pub fn record(&self, record: CheckRecord) {
        if self.tx.try_send(Message::Check(record)).is_err() {
            // Full, or the writer died. Counted rather than logged: this is the
            // path that fires hundreds of thousands of times when it fires at
            // all, and a `warn!` per record would replace a stalled journal
            // with a stalled log.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Queue one non-check event. Never blocks.
    pub fn event(&self, event: JournalEvent) {
        if self.tx.try_send(Message::Event(Box::new(event))).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Ask the writer to flush what it has. Never blocks.
    pub fn flush(&self) {
        let _ = self.tx.try_send(Message::Flush);
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Owns the writer thread, and joins it on drop.
///
/// Separate from [`Journal`] because the handle is cloned into the effect sink
/// and the thread must be joined exactly once, at shutdown, after the last
/// clone is gone — otherwise the final records are written into a file nobody
/// waited for.
pub struct JournalWriter {
    handle: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}

impl JournalWriter {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Wait for the writer to drain and stop.
    ///
    /// The caller must have dropped every [`Journal`] first; the thread stops
    /// when its channel closes, so a surviving clone would hang this.
    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(
    file: std::fs::File,
    rx: std::sync::mpsc::Receiver<Message>,
    data: Arc<MultiData>,
    names: Arc<NameTables>,
    dropped: Arc<AtomicU64>,
) {
    let mut out = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut since_flush = 0usize;
    // Reported once, at the end, rather than per record. A journal that lost
    // lines must say so *in the journal*, since that is the artifact somebody
    // reads later — a warning in a log stream this room may not even be
    // shipping is not good enough.
    let mut reported_drops = 0u64;

    loop {
        // **Only wait on a timer when there is something to wait for.** With an
        // empty buffer this blocks exactly as it always did, so an idle room
        // wakes a thread no more often than a record arrives; the tick exists
        // for a tail that is already written and not yet flushed, which is the
        // only state the timer can improve.
        let message = if since_flush == 0 {
            match rx.recv() {
                Ok(message) => message,
                Err(_) => break,
            }
        } else {
            match rx.recv_timeout(IDLE_FLUSH) {
                Ok(message) => message,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let _ = out.flush();
                    since_flush = 0;
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        match message {
            Message::Flush => {
                let _ = out.flush();
                since_flush = 0;
            }
            Message::Event(event) => {
                // Already an object; serializing it is the whole render.
                let mut line = event.as_value().to_string();
                line.push('\n');
                if out.write_all(line.as_bytes()).is_err() {
                    continue;
                }
                since_flush += 1;
                if since_flush >= FLUSH_EVERY {
                    let _ = out.flush();
                    since_flush = 0;
                }
            }
            Message::Check(record) => {
                let line = render(&record, &data, &names);
                if out.write_all(line.as_bytes()).is_err() {
                    // The disk is gone. Keep draining so the actor's sends
                    // continue to succeed rather than filling the channel and
                    // turning a dead journal into dropped-record accounting.
                    continue;
                }
                since_flush += 1;
                if since_flush >= FLUSH_EVERY {
                    let _ = out.flush();
                    since_flush = 0;
                }
            }
        }

        let lost = dropped.load(Ordering::Relaxed);
        if lost > reported_drops {
            let _ = writeln!(
                out,
                r#"{{"type":"gap","dropped":{},"note":"the journal could not keep up; this many checks are missing above"}}"#,
                lost - reported_drops
            );
            reported_drops = lost;
            // Counted like any other line. It was not, which mattered little
            // when only `FLUSH_EVERY` read this — one uncounted line in a
            // thousand — but `since_flush` is now also the test for "is there a
            // tail to flush", and a gap written as the room fell quiet would
            // have sat in the buffer with nothing left to dislodge it.
            since_flush += 1;
        }
    }

    let lost = dropped.load(Ordering::Relaxed);
    if lost > reported_drops {
        let _ = writeln!(
            out,
            r#"{{"type":"gap","dropped":{}}}"#,
            lost - reported_drops
        );
    }
    let _ = out.flush();
}

/// One JSON line, with names resolved.
///
/// Hand-rolled rather than `serde_json::to_string` on a struct: the only field
/// that can contain a character needing an escape is a name, and `escape`
/// handles those. Ids and numbers cannot, so they are written directly.
fn render(record: &CheckRecord, data: &MultiData, names: &NameTables) -> String {
    let slot_name = |slot: u32| {
        data.slot_info
            .get(&slot)
            .map(|info| info.name.as_str())
            .unwrap_or("?")
    };
    // Item names belong to the *receiver's* game, and location names to the
    // finder's. Crossing them over is the classic way to render a multiworld
    // wrong, and it looks plausible until two games share an id.
    let item_name = data
        .slot_info
        .get(&record.receiver)
        .and_then(|info| names.get(&info.game))
        .map(|game| game.item_name(record.item))
        .unwrap_or_else(|| format!("Unknown item (ID:{})", record.item));
    let location_name = data
        .slot_info
        .get(&record.finder)
        .and_then(|info| names.get(&info.game))
        .map(|game| game.location_name(record.location))
        .unwrap_or_else(|| format!("Unknown location (ID:{})", record.location));

    format!(
        r#"{{"type":"check","at":{:.3},"finder":{},"finder_name":"{}","receiver":{},"receiver_name":"{}","item":{},"item_name":"{}","location":{},"location_name":"{}","flags":{}}}"#,
        record.at,
        record.finder,
        escape(slot_name(record.finder)),
        record.receiver,
        escape(slot_name(record.receiver)),
        record.item,
        escape(&item_name),
        record.location,
        escape(&location_name),
        record.flags,
    ) + "\n"
}

/// JSON string escaping, for the four things a game's names actually contain.
///
/// Player names and item names are free text from a generator: quotes,
/// backslashes and control characters all occur. An unescaped one would make
/// the line unparseable, which in a file nobody reads until months later is the
/// worst place to find out.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_a_quote_stays_parseable() {
        assert_eq!(escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape(r"back\slash"), r"back\\slash");
        assert_eq!(escape("tab\there"), "tab\\there");
        assert_eq!(escape("bell\u{7}"), "bell\\u0007");
    }
}
