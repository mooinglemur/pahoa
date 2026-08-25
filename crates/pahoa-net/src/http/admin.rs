//! Authenticating the admin surface.
//!
//! This is a **public** interface, not a private channel that happens to have an
//! orchestrator on the other end: the room port is on a public load balancer,
//! and driving the API directly with `curl` is a capability worth keeping. The
//! consequence is that the bearer token is the only control, so three things
//! here are not optional — a token with real entropy behind it, a comparison
//! that does not leak where it stopped matching, and a limit on how fast an
//! attacker may guess.
//!
//! **A request presenting the correct token is never refused.** An earlier
//! version checked the limit before the token, on the reasoning that answering
//! identically to everyone kept the limit from becoming an oracle. It does do
//! that, and it also made this a remote denial of service against the room's own
//! orchestrator: the counter was keyed on *nothing*, so eleven wrong guesses a
//! minute from anyone who could reach the port took the whole admin surface down
//! — locks, filters and password rotations included — for everybody. No
//! credential needed, and re-trippable every window.
//!
//! Checking the token first gives that back without reopening anything. A wrong
//! guess still answers `401` and still counts, so a guesser learns exactly what
//! they learned before; what changes is that somebody else's traffic can no
//! longer lock out the holder. And brute force was never the threat model at
//! this end: the room refuses to start with a token under 32 bytes, so the
//! guessing rate is not what is protecting it.

use super::Response;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many authentication failures one source is allowed in a window before it
/// is answered `429` instead of `401`.
const FAILURE_LIMIT: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// A ceiling across every source, as anti-flood rather than anti-guessing: it
/// bounds the work a distributed guesser can make the room do, and sits far
/// above anything a misconfigured controller could reach. Like the per-source
/// limit it applies **only to the failure path**, so it can never refuse a
/// caller holding the token.
const FLOOD_LIMIT: u32 = 500;

/// How many source addresses are tracked at once.
///
/// Without a cap, the failure table is itself the attack: a spoofed source per
/// packet would grow it without bound. Expired entries are pruned first, and a
/// new source arriving at a full table is simply not tracked — it still counts
/// toward [`FLOOD_LIMIT`], which is what bounds that case.
const MAX_SOURCES: usize = 1024;

/// The bearer token, and the failure counters that bound guessing at it.
#[derive(Debug)]
pub struct Admin {
    token: String,
    failures: Mutex<Failures>,
}

#[derive(Debug)]
struct Failures {
    /// Per source, so one bad actor exhausts their own budget rather than a
    /// shared one.
    by_source: HashMap<IpAddr, Window>,
    total: Window,
}

/// A fixed window rather than a token bucket: the question is only "is this
/// hammering", and a window that resets wholesale is easier to reason about
/// than a refill rate.
#[derive(Debug)]
struct Window {
    count: u32,
    started: Instant,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            started: now,
        }
    }

    /// Count one failure, returning the running total for the current window.
    fn record(&mut self, now: Instant) -> u32 {
        if self.expired(now) {
            self.count = 0;
            self.started = now;
        }
        self.count += 1;
        self.count
    }

    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= FAILURE_WINDOW
    }
}

/// Whether a request may proceed.
pub enum Auth {
    Ok,
    /// Answer with this and go no further.
    Refused(Response),
}

impl Admin {
    /// `None` when no token is configured, which makes the whole admin surface
    /// answer `404`.
    pub fn new(token: Option<String>) -> Option<Self> {
        let token = token?;
        Some(Self {
            token,
            failures: Mutex::new(Failures {
                by_source: HashMap::new(),
                total: Window::new(Instant::now()),
            }),
        })
    }

    /// Check one request's `Authorization` header.
    ///
    /// `source` is the peer address of the TCP connection, deliberately **not**
    /// `X-Forwarded-For`: this port is reachable directly, so a forwarding
    /// header here is attacker-controlled text, and keying on it would let one
    /// caller spend everybody's budget by choosing whose it was.
    pub fn check(&self, header: Option<&str>, source: IpAddr) -> Auth {
        let supplied = header.and_then(|v| {
            let (scheme, value) = v.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("Bearer")
                .then_some(value.trim())
        });

        // The token first, and unconditionally: whoever holds it gets through
        // however much noise is arriving from elsewhere. See the module note.
        if let Some(supplied) = supplied
            && pahoa_room::secret::ct_eq(supplied.as_bytes(), self.token.as_bytes())
        {
            return Auth::Ok;
        }

        // A missing token and a wrong one are the same answer. Which of the two
        // it was is not something worth telling an attacker, and an operator can
        // see whether they sent a header.
        crate::metrics::record_auth_failure();
        if self.record_failure(source) {
            crate::metrics::record_auth_rate_limited();
            return Auth::Refused(
                Response::status(429, "Too Many Requests")
                    .with_header("Retry-After", FAILURE_WINDOW.as_secs().to_string()),
            );
        }
        Auth::Refused(
            Response::status(401, "Unauthorized").with_header("WWW-Authenticate", "Bearer"),
        )
    }

    /// Count one failure against `source` and the room, and say whether either
    /// window is now past its limit.
    fn record_failure(&self, source: IpAddr) -> bool {
        let now = Instant::now();
        let mut failures = self.failures.lock().expect("not poisoned");

        let flooding = failures.total.record(now) > FLOOD_LIMIT;

        if failures.by_source.len() >= MAX_SOURCES && !failures.by_source.contains_key(&source) {
            failures.by_source.retain(|_, window| !window.expired(now));
        }
        let full = failures.by_source.len() >= MAX_SOURCES;
        let count = match failures.by_source.entry(source) {
            Entry::Occupied(mut entry) => entry.get_mut().record(now),
            // A new source at a still-full table goes untracked rather than
            // evicting somebody: `FLOOD_LIMIT` is what covers this.
            Entry::Vacant(_) if full => 0,
            Entry::Vacant(entry) => entry.insert(Window::new(now)).record(now),
        };
        let over = count > FAILURE_LIMIT;

        if count == FAILURE_LIMIT + 1 {
            tracing::warn!(
                %source,
                limit = FAILURE_LIMIT,
                window = ?FAILURE_WINDOW,
                "too many admin authentication failures from one source; refusing its further attempts this window"
            );
        }
        if flooding && failures.total.count == FLOOD_LIMIT + 1 {
            tracing::warn!(
                sources = failures.by_source.len(),
                limit = FLOOD_LIMIT,
                window = ?FAILURE_WINDOW,
                "admin authentication failures across sources have passed the flood ceiling"
            );
        }

        over || flooding
    }

    /// How many sources are being tracked, for the test that bounds the table.
    #[cfg(test)]
    fn tracked_sources(&self) -> usize {
        self.failures.lock().expect("not poisoned").by_source.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin() -> Admin {
        Admin::new(Some("quiet-harbor-ledger".to_string())).expect("a token")
    }

    /// One source, spelled out so a test that means "somebody else" cannot
    /// accidentally reuse it.
    fn ip(s: &str) -> IpAddr {
        s.parse().expect("an address")
    }

    fn them() -> IpAddr {
        ip("198.51.100.7")
    }

    fn us() -> IpAddr {
        ip("10.42.0.3")
    }

    fn refused(auth: Auth) -> Response {
        match auth {
            Auth::Refused(r) => r,
            Auth::Ok => panic!("expected a refusal"),
        }
    }

    fn allowed(auth: Auth) -> bool {
        matches!(auth, Auth::Ok)
    }

    #[test]
    fn no_token_configured_means_no_admin_surface() {
        assert!(Admin::new(None).is_none());
    }

    #[test]
    fn the_right_token_is_accepted() {
        assert!(allowed(
            admin().check(Some("Bearer quiet-harbor-ledger"), us())
        ));
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        assert!(allowed(
            admin().check(Some("bearer quiet-harbor-ledger"), us())
        ));
        assert!(allowed(
            admin().check(Some("BEARER quiet-harbor-ledger"), us())
        ));
    }

    #[test]
    fn a_wrong_or_missing_token_is_the_same_answer() {
        let a = admin();
        for header in [None, Some("Bearer wrong"), Some("Basic abc"), Some("junk")] {
            let response = refused(a.check(header, them()));
            assert_eq!(response.status, 401, "{header:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_scheme() {
        let rendered = String::from_utf8(refused(admin().check(None, them())).render()).unwrap();
        assert!(rendered.contains("WWW-Authenticate: Bearer"), "{rendered}");
    }

    /// A prefix of the token must not be treated as the token.
    #[test]
    fn a_prefix_is_not_enough() {
        assert_eq!(
            refused(admin().check(Some("Bearer quiet"), them())).status,
            401
        );
        assert_eq!(
            refused(admin().check(Some("Bearer quiet-harbor-ledger-and-more"), them())).status,
            401
        );
    }

    #[test]
    fn guessing_is_cut_off_after_the_limit() {
        let a = admin();
        for _ in 0..FAILURE_LIMIT {
            assert_eq!(refused(a.check(Some("Bearer wrong"), them())).status, 401);
        }
        assert_eq!(refused(a.check(Some("Bearer wrong"), them())).status, 429);
    }

    /// The whole point of keying on a source: a guesser must not be able to
    /// refuse the orchestrator's calls, which is what the room-wide counter let
    /// them do with eleven requests a minute and no credential.
    #[test]
    fn a_guesser_cannot_lock_out_the_token_holder() {
        let a = admin();
        for _ in 0..FAILURE_LIMIT * 5 {
            let _ = a.check(Some("Bearer wrong"), them());
        }
        assert_eq!(refused(a.check(Some("Bearer wrong"), them())).status, 429);

        // From their own address as well as from anywhere else — holding the
        // token is what gets a caller through, not where they are.
        assert!(allowed(a.check(Some("Bearer quiet-harbor-ledger"), us())));
        assert!(allowed(a.check(Some("Bearer quiet-harbor-ledger"), them())));
    }

    /// And one bad source must not spend another's budget either, or the lockout
    /// is only moved rather than fixed.
    #[test]
    fn one_source_does_not_spend_anothers_budget() {
        let a = admin();
        for _ in 0..FAILURE_LIMIT * 2 {
            let _ = a.check(Some("Bearer wrong"), them());
        }
        assert_eq!(refused(a.check(Some("Bearer wrong"), them())).status, 429);
        assert_eq!(refused(a.check(Some("Bearer wrong"), us())).status, 401);
    }

    /// The backstop: many sources, none of them past the per-source limit.
    #[test]
    fn the_flood_ceiling_catches_what_per_source_counting_cannot() {
        let a = admin();
        let mut sent = 0;
        for host in 0..(FLOOD_LIMIT / FAILURE_LIMIT) + 1 {
            for _ in 0..FAILURE_LIMIT {
                let source = ip(&format!("198.51.100.{}", host % 256));
                let status = refused(a.check(Some("Bearer wrong"), source)).status;
                sent += 1;
                if status == 429 {
                    assert!(
                        sent > FLOOD_LIMIT,
                        "refused at {sent} failures, before the ceiling at {FLOOD_LIMIT}"
                    );
                    return;
                }
            }
        }
        panic!("{sent} failures went unrefused with a ceiling of {FLOOD_LIMIT}");
    }

    /// A spoofed source per packet must not grow the table without bound.
    #[test]
    fn the_source_table_is_bounded() {
        let a = admin();
        for host in 0..(MAX_SOURCES * 2) {
            let source = ip(&format!("2001:db8::{host:x}"));
            let _ = a.check(Some("Bearer wrong"), source);
        }
        assert!(
            a.tracked_sources() <= MAX_SOURCES,
            "tracking {} sources",
            a.tracked_sources()
        );
    }

    #[test]
    fn a_rate_limited_refusal_says_when_to_come_back() {
        let a = admin();
        for _ in 0..FAILURE_LIMIT {
            let _ = a.check(Some("Bearer wrong"), them());
        }
        let rendered = String::from_utf8(refused(a.check(None, them())).render()).unwrap();
        assert!(rendered.contains("Retry-After: 60"), "{rendered}");
    }

    /// Success must not count toward the limit, or a busy orchestrator would
    /// lock itself out.
    #[test]
    fn success_does_not_count_against_the_limit() {
        let a = admin();
        for _ in 0..100 {
            assert!(allowed(a.check(Some("Bearer quiet-harbor-ledger"), us())));
        }
        assert!(allowed(a.check(Some("Bearer quiet-harbor-ledger"), us())));
    }
}
