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
        // on a 35-game seed is 2.5 MiB against a 256 KiB share, so a client
        // that asked for the data package — which every real client does when
        // its cached checksums miss — was closed as "too slow" while sitting
        // completely idle. The budget exists to bound *accumulation*; capping
        // one legitimate payload is a correctness bug wearing its clothes.
        let queued = conn.queued.load(Ordering::Relaxed);
        let counted = queued.saturating_sub(conn.oversize.load(Ordering::Relaxed));
        let oversized = queued == 0 && size > self.per_connection;
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
        let _ = conn
            .queued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                freed = held.min(size);
                Some(held - freed)
            });
        QUEUED.fetch_sub(freed, Ordering::Relaxed);
        // An oversize allowance is only ever claimed on an empty queue, so that
        // message is first in line and the writer drains in order — the first
        // release is therefore the one that clears it.
        let _ =
            conn.oversize
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| match held {
                    0 => None,
                    held => Some(held.saturating_sub(size)),
                });
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

    /// The allowance is for a connection that is *idle*, not one already behind.
    #[test]
    fn a_backed_up_connection_gets_no_oversize_allowance() {
        let _guard = exclusive();
        let budget = Budget::new(1 << 20, 1024);
        let conn = ConnBudget::default();

        assert!(budget.reserve(&conn, 512));
        assert!(
            !budget.reserve(&conn, 4096),
            "a connection that is already behind must still be refused"
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
}
