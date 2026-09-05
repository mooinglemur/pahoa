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
use std::sync::atomic::{AtomicUsize, Ordering};

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

#[derive(Debug, Clone)]
pub struct Budget {
    limit: usize,
    per_connection: usize,
}

/// One connection's share, held by both its shard and its writer task.
#[derive(Debug, Default)]
pub struct ConnBudget {
    queued: AtomicUsize,
    /// Bytes admitted above the per-connection share by the progress guarantee
    /// below, so they do not lock out the traffic queued behind them.
    oversize: AtomicUsize,
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
        let oversized = size > self.per_connection && counted < self.per_connection;
        if !oversized && counted + size > self.per_connection {
            return false;
        }
        let total = QUEUED.fetch_add(size, Ordering::Relaxed) + size;
        if total > self.limit {
            QUEUED.fetch_sub(size, Ordering::Relaxed);
            return false;
        }
        PEAK.fetch_max(total, Ordering::Relaxed);
        conn.queued.fetch_add(size, Ordering::Relaxed);
        if oversized {
            conn.oversize.fetch_add(size, Ordering::Relaxed);
        }
        true
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
