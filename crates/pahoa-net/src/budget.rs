//! The outbound memory budget.
//!
//! # Why this is not the reference's behavior, on purpose
//!
//! Python's `websockets.broadcast` is fire-and-forget: it writes into each
//! transport buffer and never awaits, so a client that stops reading causes
//! **unbounded server-side memory growth**. At 6000 connections and a mass
//! release that is a room that dies rather than a client that does.
//!
//! Blocking the actor on a full queue would be worse still — one slow client
//! would stall the whole room. So the policy is: bound the queue, and when a
//! connection cannot keep up, **drop the connection rather than the frame**.
//!
//! # Dropping frames is not an option, and that is the subtle part
//!
//! It is tempting to skip a frame for a client that is behind and carry on.
//! That is wrong here: `send_new_items` advances the slot's `send_index` as it
//! sends, so a dropped `ReceivedItems` leaves the server believing the client
//! holds items it never received, and the client cannot tell. It would silently
//! play a different game until it happened to reconnect.
//!
//! Closing is safe precisely because the protocol is resumable — `Connect`
//! resends `checked_locations` in full and replays the item queue from index
//! zero — so a lagged client reconnects into correct state. The only thing lost
//! is chat scrollback, which any disconnect already loses.
//!
//! # Bytes, not messages
//!
//! A 140-packet `PrintJSON` chunk and a `Retrieved` reply differ by orders of
//! magnitude, so a queue bounded by message count bounds nothing in particular.
//! The cap is a **global** byte budget with a small per-connection share: at
//! 6000 connections a naive 8 MiB each would be 48 GB, so the per-connection
//! number has to stay small and the global one is what actually protects the
//! process.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Milliseconds a connection may go without a single byte reaching its socket,
/// while it is holding a backlog, before it is judged to have stopped draining.
///
/// # Why a deadline and not a depth
///
/// **Queue depth is mostly a statement about what the server just handed the
/// client, not about the client.** Answering a data-package fetch puts
/// megabytes into one connection's queue in a handful of frames; faulting it
/// for not having drained them yet blames the peer for the room's own burst,
/// and that is precisely what dropped healthy players on a live room — clients
/// closed moments after connecting, having done nothing but ask which games
/// were in it.
///
/// A client that is *draining* is keeping up however deep its queue is. A
/// client that is not draining is behind however shallow. This measures the
/// thing that distinguishes them.
///
/// Generous, because it has to exceed the time a single large frame takes to
/// reach a slow peer — progress is recorded per completed frame, so a client
/// part-way through a six-megabyte write has legitimately reported nothing yet.
/// The keepalive catches a peer that is simply gone, in a fifth of this; what
/// this catches is the rarer case of a peer that answers pings and never reads.
const STALL_DEADLINE_MS: u64 = 120_000;

/// Monotonic milliseconds since the process started.
///
/// A plain counter rather than a timestamp so it fits an atomic and so nothing
/// here depends on the wall clock, which an operator can move.
fn now_ms() -> u64 {
    /// Offset so the counter never starts near zero.
    ///
    /// Nothing in production depends on it — stall simply cannot be detected in
    /// the first `STALL_DEADLINE_MS` of a process either way, because no
    /// connection has existed long enough to have stalled. It is here so that
    /// "this connection last moved bytes two minutes ago" is expressible at all
    /// times, including in a test whose process is a millisecond old.
    const BASE: u64 = 3_600_000;

    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    BASE + START.elapsed().as_millis() as u64
}

static QUEUED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Bytes currently sitting in outbound queues across every connection.
pub fn queued_bytes() -> usize {
    QUEUED.load(Ordering::Relaxed)
}

/// High-water mark, which is the number worth watching: the budget is doing its
/// job when this stays well under the limit during a mass release.
pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Why a reservation was refused, so the log can say which bound bit.
///
/// **The distinction is the whole point.** Over its own share means one client
/// is accumulating and the room is fine; out of room budget means the room is
/// full and this client may be blameless. They are logged the same way today
/// and they are not the same problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// This connection is holding a full share of ordinary traffic.
    Share {
        counted: usize,
        size: usize,
        share: usize,
    },
    /// The room as a whole is out of outbound budget.
    RoomBudget {
        queued: usize,
        size: usize,
        limit: usize,
    },
    /// This connection has held a backlog without draining any of it. The only
    /// one of the three that is a statement about the *client*.
    Stalled { stalled_ms: u64, deadline_ms: u64 },
}

impl Refused {
    pub fn as_text(self) -> &'static str {
        match self {
            Self::Share { .. } => "over its own share",
            Self::RoomBudget { .. } => "the room is out of outbound budget",
            Self::Stalled { .. } => "it has not drained anything in two minutes",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Budget {
    limit: usize,
    per_connection: usize,
}

/// One connection's share, held by both its shard and its writer task.
#[derive(Debug)]
pub struct ConnBudget {
    queued: AtomicUsize,
    /// Bytes admitted above the per-connection share by the progress guarantee
    /// below, so they do not lock out the traffic queued behind them.
    oversize: AtomicUsize,
    /// When this connection last got bytes onto its socket, in [`now_ms`].
    ///
    /// The only evidence that separates a client which is behind from one which
    /// was simply handed a lot at once. Updated by the writer as frames
    /// complete; read here to decide whether a backlog is this client's fault.
    last_progress: AtomicU64,
}

impl Default for ConnBudget {
    /// **A new connection starts having just made progress**, which is not
    /// merely tidy. Deriving this would leave `last_progress` at zero, and the
    /// stall test reads `now - last_progress` — so on a process that had been
    /// up longer than the deadline, every connection would be born already
    /// stalled and dropped the moment anything queued for it. The failure would
    /// arrive two minutes after each restart and look exactly like the bug this
    /// whole mechanism exists to fix.
    fn default() -> Self {
        Self {
            queued: AtomicUsize::new(0),
            oversize: AtomicUsize::new(0),
            last_progress: AtomicU64::new(now_ms()),
        }
    }
}

impl ConnBudget {
    /// Record that bytes reached the socket. Called by the writer.
    pub fn made_progress(&self) {
        self.last_progress.store(now_ms(), Ordering::Relaxed);
    }

    /// Pretend the last completed write was `ms` ago.
    ///
    /// The stall deadline is minutes long and depends on a monotonic clock, so
    /// without this a test could only exercise it by waiting. Test-only, so the
    /// shipped type has no way to lie about its own progress.
    #[cfg(test)]
    fn backdate(&self, ms: u64) {
        self.last_progress
            .store(now_ms().saturating_sub(ms), Ordering::Relaxed);
    }

    /// How long this connection has held a backlog without draining any of it.
    /// `None` when it has nothing queued.
    fn stalled_for(&self) -> Option<u64> {
        if self.queued.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(now_ms().saturating_sub(self.last_progress.load(Ordering::Relaxed)))
    }
}

impl Budget {
    pub fn new(limit: usize, per_connection: usize) -> Self {
        Self {
            limit,
            per_connection,
        }
    }

    /// Claim room for a frame, or report that this connection is too far behind.
    ///
    /// Never blocks and never waits: the caller is the shard, on the path that
    /// must not stall for any one client.
    pub fn reserve(&self, conn: &ConnBudget, size: usize) -> bool {
        self.reserve_reported(conn, size).is_ok()
    }

    /// As [`reserve`](Self::reserve), but says **why** it refused.
    ///
    /// The plain boolean cost three wrong diagnoses in a row on a live room:
    /// "cannot keep up" is logged identically whether a connection is over its
    /// own share, whether the room is out of budget, or whether its writer's
    /// queue is full, and those have completely different fixes. Reading it off
    /// a log line beats reasoning about which one it must have been.
    pub fn reserve_reported(&self, conn: &ConnBudget, size: usize) -> Result<(), Refused> {
        // Per-connection first, so one client hitting its own ceiling is
        // attributed to that client rather than to whoever happens to be next
        // when the global budget runs out.
        //
        // The oversize allowance is what makes a large *single* message
        // deliverable at all. Without it any packet bigger than the share was
        // undeliverable by construction, and one routinely is: `GetDataPackage`
        // on a 35-game seed is 2.5 MiB against a 256 KiB share. The budget
        // exists to bound *accumulation*; capping one legitimate payload is a
        // correctness bug wearing its clothes.
        //
        // **The gate is "is this connection behind", not "is its queue empty".**
        // It was the second, and that made the allowance a race against the
        // writer rather than a rule: a client whose `RoomInfo` had not yet
        // reached the socket — two kilobytes, against a 256 KiB share — was
        // refused its data package and dropped as "too slow" while completely
        // idle. Seen on a live room as a reconnect loop, every client cycling
        // every few seconds, with room-wide queued bytes never above 3.28 MiB
        // of a 64 MiB budget. The busier the room the likelier it was, because
        // the ordinary feed keeps something in the queue.
        //
        // A connection that is genuinely behind is one at its share, which is
        // what `counted` measures. Anything below that is mid-drain and is
        // exactly who the allowance is for.
        let queued = conn.queued.load(Ordering::Relaxed);
        let counted = queued.saturating_sub(conn.oversize.load(Ordering::Relaxed));
        // **Large payloads accumulate; the global cap is what bounds them.**
        // Requiring the previous one to have drained was the obvious guard and
        // it was wrong for the traffic that actually exists: a client asks for
        // the data package once per game and pipelines the requests, so the
        // second arrives while the first is still on the wire. Measured on a
        // live 189-slot, 106-game room — 1,825 `GetDataPackage` across 130
        // connections, one reply of 6.19 MB against a 256 KiB share — that rule
        // closed healthy clients in a reconnect loop.
        //
        // What remains is the rule that was always doing the real work: a
        // connection may hold one share of *ordinary* traffic, and anything
        // larger than a share is not accumulation but a single legitimate
        // payload. `counted` excludes those, so a client that stops draining is
        // still caught by its ordinary backlog, and the room is still bounded
        // by `limit` — the cap documented as the one that protects the process.
        // **The judgement about the client**, as opposed to the two limits
        // below it, which are judgements about memory. A connection draining
        // steadily is keeping up no matter how deep its queue; one that has
        // moved nothing in two minutes is not, however shallow.
        if let Some(stalled) = conn.stalled_for()
            && stalled > STALL_DEADLINE_MS
        {
            return Err(Refused::Stalled {
                stalled_ms: stalled,
                deadline_ms: STALL_DEADLINE_MS,
            });
        }
        let oversized = size > self.per_connection && counted < self.per_connection;
        if !oversized && counted + size > self.per_connection {
            return Err(Refused::Share {
                counted,
                size,
                share: self.per_connection,
            });
        }
        let total = QUEUED.fetch_add(size, Ordering::Relaxed) + size;
        if total > self.limit {
            QUEUED.fetch_sub(size, Ordering::Relaxed);
            return Err(Refused::RoomBudget {
                queued: total - size,
                size,
                limit: self.limit,
            });
        }
        PEAK.fetch_max(total, Ordering::Relaxed);
        conn.queued.fetch_add(size, Ordering::Relaxed);
        if oversized {
            conn.oversize.fetch_add(size, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Give the room back, once the bytes have reached the socket.
    ///
    /// **The global counter is decremented by what this connection actually
    /// gave back, never by what the caller asked to give back**, and the two
    /// differ in a race that a live room hits constantly. A disconnecting
    /// connection is reconciled by [`release_all`] from its shard, while its
    /// writer task may already have taken a frame off the queue and be sitting
    /// in `write_all`. `release_all` counts that frame — it is still in
    /// `queued` — and the writer then releases it a second time. Subtracting
    /// blindly wraps `usize`, and because the check is `total > limit`, a
    /// counter one byte below zero reads as sixteen exabytes: every reservation
    /// in the room fails from that moment on, for every connection, forever.
    /// The room drops all of its clients at once and refuses every reconnect,
    /// while holding no memory at all.
    ///
    /// Clamping to what is held makes the two orderings agree. Whichever runs
    /// first frees the bytes; the other finds nothing left and frees nothing.
    pub fn release(conn: &ConnBudget, size: usize) {
        let mut freed = 0;
        let mut left = 0;
        let _ = conn
            .queued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                freed = held.min(size);
                left = held - freed;
                Some(left)
            });
        QUEUED.fetch_sub(freed, Ordering::Relaxed);
        if freed > 0 {
            conn.made_progress();
        }
        // **The allowance is cleared only when the connection is empty, never
        // drawn down by whatever happened to drain.**
        //
        // Subtracting each release from it looks equivalent and is not: the
        // releases are mostly *ordinary* frames, so a client holding one large
        // payload watched its allowance erode by the room's feed until
        // `counted` — which is `queued` minus the allowance — reached the whole
        // size of the payload it was still being sent. Then it was refused and
        // dropped, having drained every single frame it was ever given.
        //
        // Measured: with a 340 KB compressed data package in flight, 65 feed
        // frames of 4 KB were enough. That is seconds on a live room, which is
        // why this looked like the earlier connect-burst bug and was not — it
        // is steady state, and it is why clients kept cycling after that fix.
        //
        // Nothing is left to erode once `queued` is zero, so clearing there is
        // both correct and the only point at which the allowance is meaningless.
        if left == 0 {
            conn.oversize.store(0, Ordering::Relaxed);
        }
    }

    /// Release everything a connection still holds, when it goes away without
    /// draining. Without this the global budget leaks on every disconnect and
    /// the room slowly refuses to send anything at all.
    ///
    /// Safe to interleave with [`release`] in either order: this takes whatever
    /// is left and leaves zero behind, so a writer still owing a release for a
    /// frame counted here will find nothing to free rather than double-freeing
    /// it.
    pub fn release_all(conn: &ConnBudget) {
        let held = conn.queued.swap(0, Ordering::Relaxed);
        conn.oversize.store(0, Ordering::Relaxed);
        QUEUED.fetch_sub(held, Ordering::Relaxed);
    }
}

/// Shared handle for a connection's accounting.
pub type ConnHandle = Arc<ConnBudget>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters are process-wide and the test runner is threaded, so these
    /// have to actually exclude each other — a freshly constructed mutex would
    /// guard nothing.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        QUEUED.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        guard
    }

    /// `GetDataPackage` on a 35-game seed is 2.5 MiB against a 256 KiB share.
    /// Before the progress guarantee it could never be sent, so asking for the
    /// data package disconnected the client for being "too slow" while idle.
    #[test]
    fn one_message_larger_than_the_share_is_still_deliverable() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1024);
        let conn = ConnBudget::default();

        assert!(
            budget.reserve(&conn, 4096),
            "an idle connection must be able to make progress on any one message"
        );

        // And it must not lock out what queues up behind it: the oversize
        // message is not counted against the share for later admissions.
        assert!(budget.reserve(&conn, 512));
        assert!(budget.reserve(&conn, 512));
        assert!(!budget.reserve(&conn, 1), "the share itself still applies");
    }

    #[test]
    fn the_oversize_allowance_is_cleared_by_the_message_that_claimed_it() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1024);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 4096));
        Budget::release(&conn, 4096);
        assert_eq!(conn.queued.load(Ordering::Relaxed), 0);
        assert_eq!(conn.oversize.load(Ordering::Relaxed), 0);

        // Back to an ordinary share, with no leftover allowance.
        assert!(budget.reserve(&conn, 1024));
        assert!(!budget.reserve(&conn, 1));
    }

    /// The allowance is for a connection that is not **behind**, which is not
    /// the same as one that is *empty* — and the difference was a live bug.
    ///
    /// This test used to assert the opposite: that 512 bytes in the queue was
    /// enough to forfeit the allowance. That made admission a race against the
    /// writer rather than a rule, and on a real room it dropped healthy clients
    /// in a loop — a `RoomInfo` not yet on the wire was enough to have the data
    /// package refused and the client closed as "too slow" while idle.
    ///
    /// Behind means *at the share*. That is still refused, and that is the
    /// protection this was reaching for.
    #[test]
    fn only_a_connection_at_its_share_is_refused_the_oversize_allowance() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1024);

        // Mid-drain: a little queued, nowhere near the share.
        let draining = ConnBudget::default();
        assert!(budget.reserve(&draining, 512));
        assert!(
            budget.reserve(&draining, 4096),
            "a connection with room left in its share is not behind, and \
             refusing it here is what dropped live clients in a reconnect loop"
        );

        // Genuinely behind: nothing of its share remains.
        let stalled = ConnBudget::default();
        assert!(budget.reserve(&stalled, 1024));
        assert!(
            !budget.reserve(&stalled, 4096),
            "a connection that has used its whole share must still be refused"
        );
    }

    /// **Pipelined data-package requests, which is what real clients send.**
    ///
    /// A client asks per game and does not wait for each reply, so the second
    /// large payload arrives while the first is still on the wire. This test
    /// asserted the opposite — one in flight at a time — and that rule closed
    /// healthy clients in a reconnect loop on a live 189-slot, 106-game room.
    #[test]
    fn pipelined_large_payloads_are_all_admitted() {
        let _guard = exclusive();
        let budget = Budget::new(64 << 20, 256 * 1024);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 9_058), "RoomInfo");
        assert!(budget.reserve(&conn, 400_000), "first data package");
        assert!(
            budget.reserve(&conn, 380_000),
            "a second data package, requested before the first drained"
        );
        assert!(
            budget.reserve(&conn, 6_193_114),
            "and the whole-room package"
        );
    }

    /// **A client that drains everything it is given is never refused.**
    ///
    /// The steady-state failure, and the one that survived two earlier fixes.
    /// A connection holding one large payload — a compressed data package, say
    /// — also receives the room's ordinary feed, and each of those releases
    /// used to draw down the oversize allowance. `counted` is `queued` minus
    /// that allowance, so it climbed by 4 KB per feed frame until it reached
    /// the share and the client was dropped, having kept up with every byte.
    ///
    /// Measured on a live room: 65 frames of 4 KB was enough. Seconds of play.
    ///
    /// The loop runs far past that, and the assertion is about the client's
    /// *behavior* — it drained everything — rather than about any counter, so
    /// it holds however the accounting is rearranged underneath.
    #[test]
    fn a_client_draining_everything_is_never_refused_behind_a_large_payload() {
        let _guard = exclusive();
        let budget = Budget::new(64 << 20, 256 * 1024);
        let conn = ConnBudget::default();

        // Still on the wire for the whole test, which is the point: a big
        // write takes time, and the feed does not stop while it happens.
        assert!(budget.reserve(&conn, 340_000), "the data package");

        for frame in 0..1_000 {
            assert!(
                budget.reserve(&conn, 4_000),
                "feed frame {frame} refused after {} bytes, every one of which \
                 this client had already drained",
                frame * 4_000
            );
            Budget::release(&conn, 4_000);
        }

        // And the allowance is still doing its job at the end rather than
        // having quietly become permanent: once the payload drains, the
        // connection is back to an ordinary share.
        Budget::release(&conn, 340_000);
        assert!(budget.reserve(&conn, 262_144), "a full ordinary share");
        assert!(
            !budget.reserve(&conn, 1),
            "the share must still cap ordinary traffic afterwards"
        );
    }

    /// **A data package fetched in pieces must not drop the client.**
    ///
    /// The failure that survived three fixes, because none of them touched this
    /// path. Clients ask for the data package per game — 1,825 requests across
    /// 130 connections on the live room, about fourteen each — and each reply
    /// is its own frame. None is individually larger than the share, so none
    /// takes the oversize allowance; they simply add up, pass 256 KiB, and the
    /// client is dropped moments after connecting having asked for nothing but
    /// the names of the games.
    ///
    /// It needs no item feed, which is what ruled out every earlier
    /// explanation: the room was quiet and it still happened.
    #[test]
    fn a_data_package_fetched_in_pieces_does_not_drop_the_client() {
        let _guard = exclusive();
        // The live room: a 6.19 MB package, so this is the share it now gets.
        let share = crate::per_connection_budget_for(6_193_114);
        let budget = Budget::new(128 << 20, share);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 9_058), "RoomInfo");
        // Fourteen replies at the *compressed* size the wire actually carries —
        // 340 KB of package split fourteen ways. **Each is comfortably under
        // even the old 256 KiB share**, which is the whole point: none of them
        // is individually oversize, so none takes the allowance and every one
        // counts. Nothing drains in between because the writer is still working
        // through the first.
        const PIECE: usize = 24_000;
        const {
            assert!(
                PIECE < 256 * 1024,
                "the test must exercise the ordinary path, not the allowance"
            );
        }
        for piece in 0..14 {
            assert!(
                budget.reserve(&conn, PIECE),
                "data-package piece {piece} refused; the client is dropped for \
                 asking what games are in the room"
            );
        }

        // And uncompressed, which is what a client without deflate receives.
        let plain = ConnBudget::default();
        for piece in 0..14 {
            assert!(
                budget.reserve(&plain, 442_000),
                "uncompressed data-package piece {piece} refused"
            );
        }
    }

    /// **A connection is born having just made progress.**
    ///
    /// The stall test reads `now - last_progress`, so a zero there would mean
    /// every connection on a process older than the deadline is born already
    /// stalled, and dropped the instant anything queued for it. That failure
    /// would begin two minutes after each restart and look exactly like the bug
    /// the deadline exists to fix.
    #[test]
    fn a_new_connection_is_not_born_stalled() {
        let _guard = exclusive();
        let budget = Budget::new(64 << 20, 256 * 1024);
        let conn = ConnBudget::default();
        assert_eq!(conn.stalled_for(), None, "nothing queued yet");

        assert!(budget.reserve(&conn, 4_000));
        let stalled = conn.stalled_for().expect("now holding a backlog");
        assert!(
            stalled < 1_000,
            "a connection that has only just been created reports {stalled}ms of \
             stall, so it will be dropped as soon as anything is sent to it"
        );
    }

    /// **Depth is not the test; progress is.**
    ///
    /// A client handed megabytes in a burst — a data-package fetch — and
    /// draining every one of them is keeping up, however deep the queue got.
    /// Judging it on depth blames the peer for the room's own burst.
    #[test]
    fn a_client_that_keeps_draining_is_never_judged_behind() {
        let _guard = exclusive();
        let budget = Budget::new(128 << 20, crate::per_connection_budget_for(6_193_114));
        let conn = ConnBudget::default();

        for round in 0..200 {
            assert!(
                budget.reserve(&conn, 442_000),
                "round {round}: refused a client that has drained everything"
            );
            Budget::release(&conn, 442_000);
        }
        assert_eq!(conn.stalled_for(), None);
    }

    /// **A client that has genuinely stopped reading is still dropped**, which
    /// is what keeps the deadline from being a way to hoard the room's budget.
    ///
    /// The keepalive catches a peer that is simply gone. This catches the
    /// rarer one that answers pings — they are written straight to the socket,
    /// bypassing this queue — while never draining a byte of what it asked for.
    #[test]
    fn a_client_that_has_stopped_draining_is_refused_however_shallow_its_queue() {
        let _guard = exclusive();
        let budget = Budget::new(64 << 20, 8 << 20);
        let conn = ConnBudget::default();

        // A trivial backlog, far under any depth limit: depth is not the test.
        assert!(budget.reserve(&conn, 100));
        conn.backdate(STALL_DEADLINE_MS + 1_000);

        let refused = budget
            .reserve_reported(&conn, 100)
            .expect_err("a connection that has moved nothing must be refused");
        assert!(
            matches!(refused, Refused::Stalled { .. }),
            "refused for the wrong reason, so the log will mislead: {refused:?}"
        );
        assert_eq!(
            refused.as_text(),
            "it has not drained anything in two minutes"
        );
    }

    /// ...and one byte of progress clears it. The deadline measures movement,
    /// not how long the connection has been busy.
    #[test]
    fn any_progress_clears_the_stall() {
        let _guard = exclusive();
        let budget = Budget::new(64 << 20, 8 << 20);
        let conn = ConnBudget::default();

        // **Two frames, and only one drains.** The queue must still hold
        // something afterwards, or this passes for the wrong reason: an empty
        // queue is never stalled, so a release that empties it would clear the
        // refusal even if progress were not being recorded at all.
        assert!(budget.reserve(&conn, 4_000));
        assert!(budget.reserve(&conn, 4_000));
        conn.backdate(STALL_DEADLINE_MS + 1_000);
        assert!(budget.reserve_reported(&conn, 100).is_err());

        // The writer completes one of them; the other is still queued.
        Budget::release(&conn, 4_000);
        assert!(conn.stalled_for().is_some(), "still holding a backlog");
        assert!(
            budget.reserve(&conn, 100),
            "a connection that just got bytes onto its socket is not stalled"
        );
    }

    /// The global cap is now the only thing bounding a client that hoards large
    /// payloads, so it has to actually bind.
    #[test]
    fn the_global_cap_still_stops_a_client_hoarding_large_payloads() {
        let _guard = exclusive();
        let budget = Budget::new(4 << 20, 256 * 1024);
        let conn = ConnBudget::default();

        let mut admitted = 0;
        for _ in 0..64 {
            if budget.reserve(&conn, 1 << 20) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, 4,
            "one connection took more than the room's whole budget"
        );
    }

    #[test]
    fn the_global_limit_still_binds_an_oversize_message() {
        let _guard = exclusive();
        let budget = Budget::new(2048, 1024);
        let conn = ConnBudget::default();

        assert!(
            !budget.reserve(&conn, 4096),
            "the process-wide cap is not something one connection may exceed"
        );
        assert_eq!(
            queued_bytes(),
            0,
            "a refused reservation must leave no trace"
        );
    }

    #[test]
    fn a_connection_is_capped_by_its_own_share() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1024);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 512));
        assert!(budget.reserve(&conn, 512));
        assert!(
            !budget.reserve(&conn, 1),
            "past its share, this connection must be refused"
        );

        // And draining frees it up again.
        Budget::release(&conn, 512);
        assert!(budget.reserve(&conn, 512));
    }

    #[test]
    fn the_global_ceiling_holds_even_when_each_connection_is_within_its_share() {
        let _guard = exclusive();
        // Ten connections each entitled to 1 KiB, but only 4 KiB in total —
        // the shape that matters at 6000 connections, where per-connection
        // shares vastly oversubscribe the process.
        let budget = Budget::new(4096, 1024);
        let conns: Vec<ConnBudget> = (0..10).map(|_| ConnBudget::default()).collect();

        let mut accepted = 0;
        for conn in &conns {
            if budget.reserve(conn, 1024) {
                accepted += 1;
            }
        }
        assert_eq!(
            accepted, 4,
            "the global cap should bind before the per-connection one"
        );
        assert_eq!(queued_bytes(), 4096);
    }

    #[test]
    fn a_refused_reservation_does_not_consume_budget() {
        let _guard = exclusive();
        let budget = Budget::new(1024, 1024);
        let conn = ConnBudget::default();
        assert!(budget.reserve(&conn, 1024));
        assert!(!budget.reserve(&ConnBudget::default(), 1));
        // The failed attempt must not leave its bytes behind, or the budget
        // ratchets down until nothing can be sent.
        assert_eq!(queued_bytes(), 1024);
    }

    /// **The room-killer, in one assertion.**
    ///
    /// A connection disconnects. Its shard reconciles the whole reservation
    /// with `release_all`, but its writer task had already taken that frame off
    /// the queue and was inside `write_all`; when the write returns, the writer
    /// releases the same bytes again. Subtracting them twice wrapped the
    /// process-wide counter to just under `usize::MAX`, and since admission is
    /// `total > limit`, *every* reservation in the room failed from then on:
    /// all clients dropped in the same instant, every reconnect dropped on
    /// arrival, and the process held no memory to show for it. Seen live as
    /// `pahoa_outbound_queued_bytes 18446744073709548046` — 3570 bytes below
    /// zero was enough to end the room.
    #[test]
    fn releasing_a_frame_a_disconnect_already_reconciled_does_not_wrap() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1 << 16);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 3570));
        assert_eq!(queued_bytes(), 3570);

        // The shard sees the disconnect and hands back everything outstanding.
        Budget::release_all(&conn);
        assert_eq!(queued_bytes(), 0);

        // The writer's `write_all` returns and it releases the frame it had
        // already popped — the same bytes, a second time.
        Budget::release(&conn, 3570);

        assert_eq!(
            queued_bytes(),
            0,
            "the global counter wrapped; every reservation in the room would \
             now fail forever"
        );
    }

    /// The same race with the halves reversed, which is equally reachable: the
    /// writer finishes first and the shard reconciles afterwards.
    #[test]
    fn a_disconnect_after_the_writer_released_frees_nothing_twice() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1 << 16);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 1024));
        assert!(budget.reserve(&conn, 512));
        Budget::release(&conn, 1024);
        assert_eq!(queued_bytes(), 512);

        Budget::release_all(&conn);
        assert_eq!(
            queued_bytes(),
            0,
            "only the undrained remainder should come back"
        );
    }

    /// Partial credit, not blind subtraction: a release larger than what is
    /// held frees what is held and stops there.
    #[test]
    fn a_release_never_frees_more_than_the_connection_holds() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1 << 16);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 256));
        Budget::release(&conn, 4096);
        assert_eq!(queued_bytes(), 0);

        // And a release against a connection holding nothing is a no-op rather
        // than a hole in the global counter.
        Budget::release(&conn, 4096);
        assert_eq!(queued_bytes(), 0);
    }

    #[test]
    fn a_disconnect_returns_what_it_was_holding() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1 << 20);
        let conn = ConnBudget::default();
        budget.reserve(&conn, 4096);
        Budget::release_all(&conn);
        assert_eq!(
            queued_bytes(),
            0,
            "a dropped connection must not leak budget"
        );
    }
    /// **The live-room failure, in the shape it actually took.**
    ///
    /// Every real client requests the data package on connect when its cached
    /// checksums miss, and the reply is megabytes against a 256 KiB share. The
    /// allowance exists for exactly that — but gated on an empty queue it only
    /// fired if the writer had already drained the `RoomInfo` sent moments
    /// before, which on a busy room it usually had not.
    ///
    /// The result was a reconnect loop: connect, ask, get dropped as "cannot
    /// keep up" while completely idle, reconnect five seconds later. Room-wide
    /// queued bytes never exceeded 3.28 MiB of a 64 MiB budget, which is what
    /// ruled out the global cap and pointed here.
    #[test]
    fn a_data_package_is_deliverable_behind_an_undrained_room_info() {
        let _guard = exclusive();
        let budget = Budget::new(64 << 20, 256 * 1024);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 2_000), "RoomInfo queues fine");
        assert!(
            budget.reserve(&conn, 3_200_000),
            "the data package must reach a client that is not behind; refusing \
             it here closes an idle client as too slow"
        );
    }
}
