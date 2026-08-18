//! Authenticating the admin surface.
//!
//! This is a **public** interface, not a private channel that happens to have an
//! orchestrator on the other end: the room port is on a public load balancer,
//! and driving the API directly with `curl` is a capability worth keeping. The
//! consequence is that the bearer token is the only control, so three things
//! here are not optional — a token with real entropy behind it, a comparison
//! that does not leak where it stopped matching, and a limit on how fast an
//! attacker may guess.

use super::Response;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many authentication failures are tolerated in a window before the
/// surface stops answering at all.
const FAILURE_LIMIT: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// The bearer token, and the failure counter that bounds guessing at it.
#[derive(Debug)]
pub struct Admin {
    token: String,
    failures: Mutex<Failures>,
}

#[derive(Debug)]
struct Failures {
    count: u32,
    window_started: Instant,
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
                count: 0,
                window_started: Instant::now(),
            }),
        })
    }

    /// Check one request's `Authorization` header.
    pub fn check(&self, header: Option<&str>) -> Auth {
        // Rate limit first, and without looking at the token: a caller past the
        // limit gets the same answer whatever they sent, so the limit cannot
        // itself be used to test guesses.
        if self.rate_limited() {
            return Auth::Refused(
                Response::status(429, "Too Many Requests")
                    .with_header("Retry-After", FAILURE_WINDOW.as_secs().to_string()),
            );
        }

        let supplied = header.and_then(|v| {
            let (scheme, value) = v.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("Bearer")
                .then_some(value.trim())
        });

        match supplied {
            Some(supplied)
                if pahoa_room::secret::ct_eq(supplied.as_bytes(), self.token.as_bytes()) =>
            {
                Auth::Ok
            }
            // A missing token and a wrong one are the same answer. Which of the
            // two it was is not something worth telling an attacker, and an
            // operator can see whether they sent a header.
            _ => {
                self.record_failure();
                Auth::Refused(
                    Response::status(401, "Unauthorized").with_header("WWW-Authenticate", "Bearer"),
                )
            }
        }
    }

    /// A fixed window rather than a token bucket: the question is only "is
    /// something hammering this", and a window that resets wholesale is easier
    /// to reason about than a refill rate.
    fn rate_limited(&self) -> bool {
        let mut failures = self.failures.lock().expect("not poisoned");
        if failures.window_started.elapsed() >= FAILURE_WINDOW {
            failures.count = 0;
            failures.window_started = Instant::now();
        }
        failures.count >= FAILURE_LIMIT
    }

    fn record_failure(&self) {
        let mut failures = self.failures.lock().expect("not poisoned");
        if failures.window_started.elapsed() >= FAILURE_WINDOW {
            failures.count = 0;
            failures.window_started = Instant::now();
        }
        failures.count += 1;
        if failures.count == FAILURE_LIMIT {
            tracing::warn!(
                failures = failures.count,
                window = ?FAILURE_WINDOW,
                "too many admin authentication failures; refusing further attempts this window"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin() -> Admin {
        Admin::new(Some("quiet-harbor-ledger".to_string())).expect("a token")
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
        assert!(allowed(admin().check(Some("Bearer quiet-harbor-ledger"))));
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        assert!(allowed(admin().check(Some("bearer quiet-harbor-ledger"))));
        assert!(allowed(admin().check(Some("BEARER quiet-harbor-ledger"))));
    }

    #[test]
    fn a_wrong_or_missing_token_is_the_same_answer() {
        let a = admin();
        for header in [None, Some("Bearer wrong"), Some("Basic abc"), Some("junk")] {
            let response = refused(a.check(header));
            assert_eq!(response.status, 401, "{header:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_scheme() {
        let rendered = String::from_utf8(refused(admin().check(None)).render()).unwrap();
        assert!(rendered.contains("WWW-Authenticate: Bearer"), "{rendered}");
    }

    /// A prefix of the token must not be treated as the token.
    #[test]
    fn a_prefix_is_not_enough() {
        assert_eq!(refused(admin().check(Some("Bearer quiet"))).status, 401);
        assert_eq!(
            refused(admin().check(Some("Bearer quiet-harbor-ledger-and-more"))).status,
            401
        );
    }

    #[test]
    fn guessing_is_cut_off_after_the_limit() {
        let a = admin();
        for _ in 0..FAILURE_LIMIT {
            assert_eq!(refused(a.check(Some("Bearer wrong"))).status, 401);
        }
        let response = refused(a.check(Some("Bearer wrong")));
        assert_eq!(response.status, 429);

        // And the limit does not become an oracle: the *correct* token is
        // refused just the same while the window is closed.
        assert_eq!(
            refused(a.check(Some("Bearer quiet-harbor-ledger"))).status,
            429
        );
    }

    #[test]
    fn a_rate_limited_refusal_says_when_to_come_back() {
        let a = admin();
        for _ in 0..FAILURE_LIMIT {
            let _ = a.check(Some("Bearer wrong"));
        }
        let rendered = String::from_utf8(refused(a.check(None)).render()).unwrap();
        assert!(rendered.contains("Retry-After: 60"), "{rendered}");
    }

    /// Success must not count toward the limit, or a busy orchestrator would
    /// lock itself out.
    #[test]
    fn success_does_not_count_against_the_limit() {
        let a = admin();
        for _ in 0..100 {
            assert!(allowed(a.check(Some("Bearer quiet-harbor-ledger"))));
        }
        assert!(allowed(a.check(Some("Bearer quiet-harbor-ledger"))));
    }
}
