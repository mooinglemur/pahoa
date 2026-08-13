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
        if conn.queued.load(Ordering::Relaxed) + size > self.per_connection {
            return false;
        }
        let total = QUEUED.fetch_add(size, Ordering::Relaxed) + size;
        if total > self.limit {
            QUEUED.fetch_sub(size, Ordering::Relaxed);
            return false;
        }
        PEAK.fetch_max(total, Ordering::Relaxed);
        conn.queued.fetch_add(size, Ordering::Relaxed);
        true
    }

    /// Give the room back, once the bytes have reached the socket.
    pub fn release(conn: &ConnBudget, size: usize) {
        conn.queued.fetch_sub(size, Ordering::Relaxed);
        QUEUED.fetch_sub(size, Ordering::Relaxed);
    }

    /// Release everything a connection still holds, when it goes away without
    /// draining. Without this the global budget leaks on every disconnect and
    /// the room slowly refuses to send anything at all.
    pub fn release_all(conn: &ConnBudget) {
        let held = conn.queued.swap(0, Ordering::Relaxed);
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
