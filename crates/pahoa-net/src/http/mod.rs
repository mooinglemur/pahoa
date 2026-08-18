//! The HTTP surface, on the same port as the game.
//!
//! One port serves the WebSocket feed, the readiness probe, the public room
//! description and the admin API. `accept` decides which of the two a
//! connection is; everything under this module handles the HTTP half.
//!
//! **A router is something a listener is given, not something wired into one.**
//! The scoped feed (`docs/scoped-feed.md`) is a second port serving a filtered
//! WebSocket feed and *the same* HTTP surface, so a router that assumed it
//! belonged to one listener would have to be unpicked to get there. It is an
//! `Arc` shared between however many listeners exist.
//!
//! No framework. `httparse` was already here for the WebSocket handshake and
//! `serde_json` for the protocol, and the whole surface is a handful of routes
//! answering small JSON documents.

mod admin;
mod command;
mod response;
mod status;

pub use admin::Admin;
pub use response::Response;
pub use status::{SlotStatus, Status};

use crate::actor::ActorMsg;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{mpsc, oneshot};

/// Everything the HTTP surface needs, shared across every listener.
#[derive(Clone)]
pub struct Router(Arc<Inner>);

struct Inner {
    seed: Arc<pahoa_multidata::MultiData>,
    actor: mpsc::Sender<ActorMsg>,
    started_at: SystemTime,
    /// `None` when no token is configured, which makes the admin surface 404.
    admin: Option<Admin>,
    /// Reported so an operator can compare what the room is holding against
    /// what it is allowed to hold.
    outbound_budget_bytes: usize,
    /// Fired by `POST /admin/v1/shutdown`, and awaited by whatever owns the
    /// process's exit.
    shutdown: Arc<tokio::sync::Notify>,
}

impl Router {
    pub fn new(
        seed: Arc<pahoa_multidata::MultiData>,
        actor: mpsc::Sender<ActorMsg>,
        config: &crate::NetConfig,
        shutdown: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self(Arc::new(Inner {
            seed,
            actor,
            started_at: SystemTime::now(),
            admin: Admin::new(config.admin_token.clone()),
            outbound_budget_bytes: config.outbound_budget_bytes,
            shutdown,
        }))
    }

    /// Answer one request.
    ///
    /// Takes the whole exchange rather than a path, because a route may want the
    /// method, a header or the body — and returns a [`Response`] rather than
    /// writing, so the routing is testable without a socket.
    pub async fn route(&self, exchange: &crate::ws::accept::Exchange) -> Response {
        let request = &exchange.request;
        let method = request.method.as_str();

        // Matched on the path exactly, with any query string cut off. Nothing
        // here takes a query parameter yet; when something does, it parses its
        // own.
        let path = request.path.split(['?', '#']).next().unwrap_or("/");

        // The admin surface is *absent* rather than locked when no token is
        // configured, so a misconfiguration fails closed and is
        // indistinguishable from an old build that never had one.
        if path.starts_with("/admin/v1/") {
            let Some(admin) = &self.0.admin else {
                return Response::not_found();
            };
            if let admin::Auth::Refused(response) = admin.check(request.header("Authorization")) {
                return response;
            }
            return self.admin_route(method, path, &exchange.body).await;
        }

        match (method, path) {
            ("GET", "/healthz") => Response::text(200, "ok\n"),
            ("GET", "/api/v1/room") => self.room().await,
            // A path that exists but not for this verb is worth distinguishing
            // from one that does not exist at all.
            (_, "/healthz" | "/api/v1/room") => Response::status(405, "Method Not Allowed"),
            _ => Response::not_found(),
        }
    }

    /// Authenticated already, by the time anything here runs.
    async fn admin_route(&self, method: &str, path: &str, body: &[u8]) -> Response {
        // Before the exact matches, because this route carries a slot number in
        // the path rather than in the body.
        if let Some(slot) = slot_password_path(path) {
            return match method {
                "POST" => self.set_slot_password(slot, body).await,
                _ => Response::status(405, "Method Not Allowed"),
            };
        }

        match (method, path) {
            ("GET", "/admin/v1/status") => self.status().await,
            ("GET", "/admin/v1/metrics") => self.metrics().await,
            ("POST", "/admin/v1/command") => self.command(body).await,
            ("POST", "/admin/v1/shutdown") => self.shutdown(),
            (
                _,
                "/admin/v1/status" | "/admin/v1/metrics" | "/admin/v1/command"
                | "/admin/v1/shutdown",
            ) => Response::status(405, "Method Not Allowed"),
            _ => Response::not_found(),
        }
    }

    /// Run one typed command against the room.
    async fn command(&self, body: &[u8]) -> Response {
        let command = match command::parse(body) {
            Ok(command) => command,
            // A command that cannot be understood is the caller's mistake, and
            // naming the part that is wrong is what makes it fixable.
            Err(detail) => {
                return Response::json(
                    400,
                    &serde_json::json!({
                        "ok": false,
                        "output": [detail],
                        "affected_slots": [],
                    }),
                );
            }
        };

        let (tx, rx) = oneshot::channel();
        if self
            .0
            .actor
            .send(ActorMsg::Admin { command, reply: tx })
            .await
            .is_err()
        {
            return stopping();
        }
        let Ok(outcome) = rx.await else {
            return stopping();
        };

        // A command the *room* refused is still a request that was understood
        // and answered, so it is a 200 carrying `ok: false` rather than a 4xx.
        // The caller renders `output` either way, and only a malformed request
        // is its own fault.
        Response::json(
            200,
            &serde_json::json!({
                "ok": outcome.ok,
                "output": outcome.output,
                "affected_slots": outcome.affected_slots,
            }),
        )
    }

    /// Rotate one slot's password on a live room.
    async fn set_slot_password(&self, slot: u32, body: &[u8]) -> Response {
        let password = match command::slot_password(body) {
            Ok(password) => password,
            Err(detail) => return Response::json(400, &serde_json::json!({"error": detail})),
        };

        let (tx, rx) = oneshot::channel();
        if self
            .0
            .actor
            .send(ActorMsg::SetSlotPassword {
                slot,
                password,
                reply: tx,
            })
            .await
            .is_err()
        {
            return stopping();
        }
        match rx.await {
            Ok(true) => Response::json(200, &serde_json::json!({"ok": true, "slot": slot})),
            Ok(false) => Response::json(
                404,
                &serde_json::json!({"error": format!("there is no slot {slot} in this seed")}),
            ),
            Err(_) => stopping(),
        }
    }

    async fn status(&self) -> Response {
        let Some(live) = self.query().await else {
            return stopping();
        };
        Response::json(
            200,
            &status::document(
                &self.0.seed,
                &live,
                self.0.started_at,
                self.0.outbound_budget_bytes,
            ),
        )
    }

    async fn metrics(&self) -> Response {
        let Some(live) = self.query().await else {
            return stopping();
        };
        Response::prometheus(status::prometheus(&live, self.0.outbound_budget_bytes))
    }

    /// Ask the process to stop, the same way SIGTERM does.
    ///
    /// Answers before quiescing rather than after: the room then closes every
    /// connection, including this one, so a response written afterwards would
    /// race the socket it was going to be written to.
    fn shutdown(&self) -> Response {
        tracing::info!("shutdown requested through the admin API");
        self.0.shutdown.notify_waiters();
        Response::text(202, "shutting down\n")
    }

    /// The full walk, for the admin surface.
    async fn query(&self) -> Option<Status> {
        let (tx, rx) = oneshot::channel();
        self.0
            .actor
            .send(ActorMsg::Status { reply: tx })
            .await
            .ok()?;
        rx.await.ok()
    }

    /// What a room page shows. Public, and therefore carries no secrets and no
    /// per-slot progress — only what the seed already tells anyone holding it.
    async fn room(&self) -> Response {
        let seed = &self.0.seed;
        let live = self.live().await;

        let slots: Vec<serde_json::Value> = seed
            .player_slots()
            .map(|(number, info)| {
                serde_json::json!({
                    "slot": number,
                    "name": info.name,
                    "game": info.game,
                    "total_checks": seed.locations.count_for(*number),
                })
            })
            .collect();

        Response::json(
            200,
            &serde_json::json!({
                "seed_name": seed.seed_name,
                "pahoa_version": env!("CARGO_PKG_VERSION"),
                "api_version": 1,
                "started_at": rfc3339(self.0.started_at),
                "password": live.as_ref().map(|l| l.password_required),
                "clients_connected": live.as_ref().map(|l| l.clients_connected),
                "slots": slots,
            }),
        )
    }

    /// Ask the actor for the handful of live numbers this surface reports.
    ///
    /// `None` when the room has stopped — the listener can outlive the actor
    /// during shutdown, and reporting zero would be a lie that reads as an idle
    /// room rather than a stopping one.
    async fn live(&self) -> Option<Live> {
        let (tx, rx) = oneshot::channel();
        self.0.actor.send(ActorMsg::Live { reply: tx }).await.ok()?;
        rx.await.ok()
    }
}

/// The live figures the public surface reports.
#[derive(Debug, Clone, Copy)]
pub struct Live {
    pub clients_connected: usize,
    pub password_required: bool,
}

/// Match `/admin/v1/slots/<n>/password`, returning the slot number.
///
/// Hand-matched rather than pattern-routed: this is the only route in the whole
/// surface with a variable in its path, and a router generic enough to express
/// it would be more machinery than the one case is worth.
fn slot_password_path(path: &str) -> Option<u32> {
    let rest = path.strip_prefix("/admin/v1/slots/")?;
    let slot = rest.strip_suffix("/password")?;
    // Rejects `12/34`, `-1`, and an empty segment, all of which `parse` would
    // otherwise have to be trusted to catch.
    slot.parse().ok()
}

/// The room has stopped answering, which during a shutdown is ordinary.
///
/// `503` rather than an empty document, because a monitor that read zeros here
/// would record a healthy idle room at exactly the moment one was going away.
fn stopping() -> Response {
    Response::text(503, "the room is stopping\n")
}

/// `2026-08-17T12:00:00Z`, hand-rolled.
///
/// The only timestamps this server formats are a handful in one JSON document,
/// and a date library for that would be a dependency doing arithmetic that fits
/// in twenty lines. Civil-from-days is Howard Hinnant's algorithm.
pub fn rfc3339(at: SystemTime) -> String {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> String {
        rfc3339(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(1), "1970-01-01T00:00:01Z");
        // A leap day, and the day after it.
        assert_eq!(at(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(at(1_709_251_200), "2024-03-01T00:00:00Z");
        // 2000 is a leap year, 1900 was not — the century rule both ways.
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        // The handoff's own example value.
        assert_eq!(at(1_786_968_000), "2026-08-17T12:00:00Z");
        // End of a day, to catch an off-by-one in the hour split.
        assert_eq!(at(1_786_968_000 + 43_199), "2026-08-17T23:59:59Z");
    }
}
