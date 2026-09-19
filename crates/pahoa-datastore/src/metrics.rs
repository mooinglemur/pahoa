//! What data storage costs, for the room's metrics endpoint.
//!
//! Process-global counters, like the rest of pahoa's metrics: a process is one
//! room, so there is nothing to key them by.
//!
//! **These exist because the expensive part of a `Set` is invisible from
//! outside.** `pahoa_packets_in_total{cmd="Set"}` counts packets, and a packet
//! that spends two hundred milliseconds in [`crate::apply_all`] looks exactly
//! like one that spends fifty nanoseconds. That matters more here than
//! elsewhere: these operations run on the actor task, the single thread that
//! owns all room state, so their cost is not paid by the client that asked for
//! it: it is paid by everybody.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

/// Failures by operation name. Sparse: an operation that has never failed has
/// no entry, because a gap and a zero say different things on a dashboard.
///
/// **Cardinality is bounded by construction.** The key is only ever one of the
/// eighteen operation names or the literal `"unknown"` (see [`record_failure`])
/// so a client cannot mint series by inventing operations.
static FAILURES: LazyLock<RwLock<HashMap<String, AtomicU64>>> = LazyLock::new(RwLock::default);

static APPLIED: AtomicU64 = AtomicU64::new(0);
static APPLY_NANOS: AtomicU64 = AtomicU64::new(0);
static APPLY_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

/// Count one failed sequence against the operation that failed it.
///
/// `op` must be an operation name the dispatcher recognized, or `"unknown"`.
/// [`crate::apply_all`] is the only caller and it enforces that: an
/// unrecognized name arrives as [`crate::OpError::UnknownOperation`] and is
/// labelled `"unknown"` rather than echoed, because the name came from the
/// client and would otherwise be unbounded label cardinality.
pub fn record_failure(op: &str) {
    if let Some(count) = FAILURES.read().expect("not poisoned").get(op) {
        count.fetch_add(1, Ordering::Relaxed);
        return;
    }
    FAILURES
        .write()
        .expect("not poisoned")
        .entry(op.to_string())
        .or_default()
        .fetch_add(1, Ordering::Relaxed);
}

/// Every operation that has ever failed, with its count.
pub fn failures() -> Vec<(String, u64)> {
    FAILURES
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(op, count)| (op.clone(), count.load(Ordering::Relaxed)))
        .collect()
}

/// Record what one call to [`crate::apply_all`] cost.
pub fn record_apply(elapsed: Duration) {
    let nanos = elapsed.as_nanos().min(u64::MAX as u128) as u64;
    APPLIED.fetch_add(1, Ordering::Relaxed);
    APPLY_NANOS.fetch_add(nanos, Ordering::Relaxed);
    APPLY_MAX_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

/// How much time the actor has spent applying operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApplyStats {
    /// Sequences applied, successful or not.
    pub applied: u64,
    pub total: Duration,
    /// The worst single sequence since the room started.
    ///
    /// **A high-water mark, not a histogram, and it only catches a stall that
    /// ended.** An operation that never returns (which is what
    /// `[] * 10**12` did before it was fixed) records nothing at all, because
    /// there is no "after" to subtract from. The thing that shows *that* is
    /// `pahoa_mailbox_depth` climbing and never draining.
    pub max: Duration,
}

pub fn apply_stats() -> ApplyStats {
    ApplyStats {
        applied: APPLIED.load(Ordering::Relaxed),
        total: Duration::from_nanos(APPLY_NANOS.load(Ordering::Relaxed)),
        max: Duration::from_nanos(APPLY_MAX_NANOS.load(Ordering::Relaxed)),
    }
}

/// The exact length of a value's compact JSON encoding, without building it.
///
/// `serde_json::Value`'s `Display` is the same compact form the wire and the
/// save use, so writing it into a sink that only counts gives the true byte
/// count for no allocation. Worth the care: this runs on the actor for every
/// `Set`, and the values it measures can be megabytes.
pub fn json_len(value: &serde_json::Value) -> usize {
    use std::fmt::Write as _;
    let mut sink = Counter(0);
    // `Display` for `Value` is infallible and `Counter` never errors.
    let _ = write!(sink, "{value}");
    sink.0
}

struct Counter(usize);

impl std::fmt::Write for Counter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_len_agrees_with_actually_encoding_it() {
        // Including the cases where a cheaper estimate would drift: escapes,
        // non-ASCII, nesting, and numbers whose text is kept verbatim.
        for value in [
            json!(null),
            json!(0),
            json!(-17.5),
            json!("ab"),
            json!("quote\" backslash\\ newline\n"),
            json!("héllo ☃"),
            json!([]),
            json!({}),
            json!([1, [2, [3, {"k": "v"}]]]),
            json!({"a": [1, 2, 3], "b": {"c": true}}),
            serde_json::from_str("2361183241434822606849").unwrap(),
        ] {
            assert_eq!(
                json_len(&value),
                serde_json::to_string(&value).unwrap().len(),
                "{value}"
            );
        }
    }

    #[test]
    fn the_worst_apply_is_remembered_and_the_total_accumulates() {
        let before = apply_stats();
        record_apply(Duration::from_millis(5));
        record_apply(Duration::from_millis(1));
        let after = apply_stats();

        assert_eq!(after.applied, before.applied + 2);
        assert!(after.total >= before.total + Duration::from_millis(6));
        assert!(
            after.max >= Duration::from_millis(5),
            "the 1ms call must not lower the high-water mark"
        );
    }
}
