# Handoff: `puna` → `pahoa`

Changes pahoa needs so that **puna** — the Kubernetes room orchestrator and web app being written at
`/home/troy/src/puna` — can provision, observe and manage pahoa rooms. Every claim about current
behavior below was read from this tree at `714d4b7`, with file and line, so it can be confirmed
rather than trusted. **Current state first; required changes after; unverified claims marked.**

Last updated 2026-08-17, before any puna code exists.

One framing point that shapes several decisions: **puna is one client of pahoa's API, not a
gatekeeper.** The room port is on a public LoadBalancer, and the intent is that a technical operator
can drive the admin API directly with `curl`. So the API is worth designing as a public interface,
not as a private channel that happens to have puna on the other end.

---

## Current state — what puna is coding against today

| Property | Where | Consequence for puna |
|---|---|---|
| Config is **argv only**; zero `env::var` in non-test code | `crates/pahoa/src/main.rs:75-94` (`SERVE_OPTS`) | Secrets would be world-readable in the pod spec |
| Unknown **or repeated** option ⇒ exit 1 | `crates/pahoa/src/cli.rs` | A typo in puna's arg builder is an unstartable room |
| Save dir holds exactly `room.save` + `room.lock` | `crates/pahoa-net/src/save.rs:66,69` | Puna writes only the seed into that directory |
| Exclusive non-blocking `flock`, `AddrInUse` + exit 1 on contention | `save.rs:85-92` | Forces `strategy: Recreate` on room Deployments |
| Lock + restore happen **before** the listener binds | `crates/pahoa/src/serve.rs` | A TCP probe succeeding means the room is really loaded |
| Non-upgrade HTTP gets `400`; a test asserts it for `GET /health` | `crates/pahoa-net/src/ws/accept.rs:145-165`, test at `:299-309` | No HTTP readiness probe is possible yet |
| Shutdown awaits **SIGINT only** | `serve.rs:132` | Under k8s the final save never runs |
| No tracing subscriber anywhere | (absent) | Container logs are 5 `println!` lines |
| `metrics.rs` counters exist and **nothing calls them** | `crates/pahoa-net/src/metrics.rs` | No status, no client count, no RSS |
| Worker threads sized from the **cgroup CPU quota** | `crates/pahoa-net/src/config.rs:81`, `:154-168` | Puna must always set `limits.cpu` |
| `outbound_budget_for(slots)` = `max(64 MiB, slots·3·96 KiB)` | `config.rs:122-133` | Puna's `limits.memory` heuristic |
| No filtered second listener; no `--filtered-port` | `main.rs:75-94` | Puna reserves the port pair but publishes one |
| Remote admin is a stub | `crates/pahoa-room/src/room/commands.rs:811,818` | No console is possible yet |
| **No CI, no image, no registry** | (absent) | Blocks everything — see below |
| **No TLS crate at all**, not even in `Cargo.lock` | (absent) | See P14; this is a bigger change than it looks |

There is a real command dispatcher already — `commands.rs:185-206` handles `release`, `collect`,
`countdown`, `hint` and more, writing into an `out` sink. It is the right foundation for the admin
command set, but **every handler is connection-scoped**: `cmd_release(conn, out)` releases *the
caller's* slot. An admin request has no connection, and "release" from a console means "release slot
N". So P13 supplies the target rather than inferring it — reusing the `cmd_*` internals, not their
entry points.

---

## The blocker

**P10 — pahoa has no CI and no published image.** No `.gitlab-ci.yml`, no `.github/`, nothing in
`registry.git.mooinglemur.com`. Puna's pod spec pins `pahoa:sha-<commit>` and there is nothing to
pin, so this gates puna's M7 (its first real Kubernetes call) and everything after it.

The good news is it is one job. The `Dockerfile` is already self-contained multi-stage — builder,
a `verify` stage that runs `pahoa selftest` and asserts a static ELF, then `FROM scratch`. That is
simpler than the lobby's pipeline, which builds outside the image and stages a `build-result`
binary. Follow `/home/troy/src/Archipelago-lobby/.gitlab-ci.yml` for the buildah invocation and the
tagging convention: always `:sha-<short sha>`, `main` → `:latest`, `ionium-dev` → `:dev`.

**Start this first.** It is independent of every other item and it is the one most likely to be
forgotten until it blocks someone.

---

## Contracts puna is coded against

These are written here verbatim so the two implementations cannot drift. Puna's copy lives at
`puna/docs/pahoa-admin-api.md`.

### Secrets arrive through the environment

Puna renders a per-room Kubernetes Secret and attaches it with `envFrom`, so the pod spec contains
only a reference to it rather than any value.

| Variable | When puna sets it | Meaning |
|---|---|---|
| `PAHOA_ADMIN_TOKEN` | always | Bearer token for `/admin/v1/**`. Absent ⇒ the admin surface **404s** |
| `PAHOA_PASSWORD` | room-wide password mode | Equivalent to today's `--password` |
| `PAHOA_SERVER_PASSWORD` | rarely | Equivalent to today's `--server-password` |
| `PAHOA_SLOT_PASSWORDS` | per-slot password mode | JSON, flat, slot number as the key |

`PAHOA_SLOT_PASSWORDS` shape — JSON object keys are strings, so the slot number is quoted:

```json
{"1": "quiet-harbor-ledger", "2": "amber-ferry-quartz", "7": "…"}
```

Slots absent from the object have no password. **Per-slot passwords are optional and so is the
room-wide one**: puna supports three mutually exclusive modes — passwordless, one room password (as
reference Archipelago), or per-slot. It guarantees at most one of `PAHOA_PASSWORD` /
`PAHOA_SLOT_PASSWORDS` is set, but pahoa should still **error at startup if both are present**
rather than silently preferring one.

Paths are not secret and should stay argv: `--save-dir`, `--snapshot`, `--tls-cert`, `--tls-key`.

**Suggestion on the existing argv flags:** keep `--password` and `--server-password` working, since
pahoa is also a tool someone runs by hand, but let the environment take precedence and log a warning
when a secret arrives via argv. Removing them outright is a breaking change with no upside.

### The HTTP surface, all on the one room port

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/healthz` | none | 200 once the save is restored and the listener is up |
| GET | `/api/v1/room` | none | Public, no secrets — what a room page shows |
| GET | `/tracker/…` | none or per-slot | The `archipelago.gg/tracker/...` analogue |
| GET | `/admin/v1/status` | bearer | Clients, activity, save, net metrics, per-slot |
| GET | `/admin/v1/metrics` | bearer | Prometheus text exposition of `metrics.rs` |
| POST | `/admin/v1/command` | bearer | The typed command set below |
| POST | `/admin/v1/save` | bearer | Force a save now |
| POST | `/admin/v1/slots/<n>/password` | bearer | Rotate one slot without a restart |
| POST | `/admin/v1/shutdown` | bearer | Quiesce, save, release the flock, exit 0 |
| (ws) | `/` | game protocol | The multiworld socket, `wss://` |

`/admin/v1/status`, whose `net` block is a one-to-one mapping onto functions in `metrics.rs` that
nothing currently calls:

```json
{ "seed_name": "56807069331869547085", "pahoa_version": "0.1.0", "api_version": 1,
  "started_at": "2026-08-17T12:00:00Z",
  "save": { "last_save_at": "…", "last_save_bytes": 481203, "last_save_micros": 4211,
            "save_interval_seconds": 30, "dirty": false },
  "net":  { "clients_connected": 7, "mailbox_depth": 0, "mailbox_peak": 13,
            "lag_disconnects": 0, "outbound_queued_bytes": 0,
            "outbound_budget_bytes": 67108864, "resident_bytes": 132018176 },
  "activity": { "last_client_message_at": "…", "idle_seconds": 109 },
  "slots": [ { "slot": 1, "name": "Troy", "game": "A Link to the Past", "connected": true,
               "checks": 34, "total_checks": 216, "status": "playing" } ] }
```

### Typed commands, with an explicit target slot

`POST /admin/v1/command`, a tagged enum rather than a command line — see *Why these shapes* below.

| Command | Body |
|---|---|
| `status` | `{"command":"status"}` |
| `say` | `{"command":"say","text":"…"}` |
| `hint` | `{"command":"hint","slot":3,"item":"Progressive Sword","force":false}` |
| `countdown` | `{"command":"countdown","seconds":10}` |
| `release` | `{"command":"release","slot":3}` |
| `collect` | `{"command":"collect","slot":3}` |
| `send_item` | `{"command":"send_item","slot":3,"item":"…"}` |
| `kick` | `{"command":"kick","slot":3,"reason":"…"}` |

Response, for every command:

```json
{"ok": true, "output": ["Released 12 items for Troy"], "affected_slots": [3]}
```

`output` lines are rendered verbatim in puna's console pane, so pahoa's own phrasing is what an
organizer reads. Puna tiers these itself — `status`/`say`/`hint`/`countdown` for room helpers, the
rest for organizers — so pahoa needs no notion of roles. An unknown command should be a `400`.

### Error conventions

- `401` with `WWW-Authenticate: Bearer` on a bad or missing token.
- **`404` when `PAHOA_ADMIN_TOKEN` is unset** — the admin surface is *absent*, not merely locked, so a
  misconfiguration fails closed and is indistinguishable from an old build.
- Because the mutating surface is internet-reachable by design, three things are not optional: at
  least 32 bytes of CSPRNG entropy in the token, **constant-time comparison**, and rate-limiting on
  authentication failure.

---

## Constraints puna depends on — do not change these silently

Each of these is load-bearing on puna's side. Breaking one produces a failure that will not look like
it came from pahoa.

1. **Passwords must not be persisted into `room.save`.** The environment is authoritative on every
   start. If a copy lives in the save file it will shadow the configured value, and rotation will
   appear to work and then revert on the next restart.
2. **The listener binds only after the save is restored.** Puna's readiness probe means "this room is
   really serving", not "the process started". Binding earlier would make cold-start detection lie.
3. **A second process on one save directory must keep failing fast.** `save.rs:85-92` is the last
   backstop against two pods serving one room, and it is why puna sets `strategy: Recreate` rather
   than trusting its own leader election alone.
4. **Worker threads must keep coming from the cgroup quota**, not `available_parallelism()`. Puna
   always sets `limits.cpu` on the strength of `config.rs:154-168`; if that reverts to host
   parallelism, a five-slot room on a 32-core node spawns 32 workers.
5. **`outbound_budget_for` stays the sizing signal.** Puna derives `limits.memory` from it and from
   the seed's slot count. Reporting `resident_bytes` (P6) is what lets that heuristic be replaced by
   measurement.
6. **The startup line keeps its shape.** `serve.rs:123` is currently the only machine-readable
   evidence a room came up.

---

## Required changes

Ordered by what unblocks what, with the puna milestone each gates.

| # | Change | Gates |
|---|---|---|
| **P10** | **CI and a published image.** One buildah job over the existing multi-stage `Dockerfile`, tagging `:sha-<short sha>`. | **puna M7 — hard blocker** |
| **P1** | **Handle SIGTERM.** `serve.rs:132` awaits only `tokio::signal::ctrl_c()`, which is SIGINT. Kubernetes sends SIGTERM and then SIGKILLs, so `server.shutdown()` and the final save flush never run and every teardown silently loses up to `--save-interval` of play. `select!` on SIGINT and `SignalKind::terminate()`. | correctness, any time |
| **P2** | **Install a tracing subscriber.** There is none, so every `tracing::` call in the shipped binary is discarded — including `save failed; its recovery point is stale` and lag disconnects. Prefer `--log-level` as a flag over `RUST_LOG`, to keep the argv-only rule for non-secrets. Do this early: it makes every later item debuggable. | operability, early |
| **P3 / P11** | **Read secrets from the environment**, per the contract above, and error if both password modes are set. | puna M7 |
| **P14** | **Terminate TLS on the room port**, from `--tls-cert` / `--tls-key`, **reloading on file change**. See the note below — this is the largest item. | puna M8, M10 |
| **P4** | **Serve HTTP(S) on the listener** — `/healthz`, `/api/v1/room`, `/admin/v1/**`, `/tracker/…`, keyed by path. `accept.rs` already parses the request line and holds `request.path`, so this is a local change rather than a framework adoption. The test at `accept.rs:299-309` asserting `400` for `GET /health` must change. | puna M11 |
| **P8** | **`POST /admin/v1/shutdown`** — stop accepting, broadcast a `Print` telling clients the room is closing, force a save, fsync, drop the `SaveStore` to release the flock, exit 0. | puna M11 |
| **P6** | **Expose `metrics.rs`** through `/admin/v1/status` and `/admin/v1/metrics`. | puna M11 |
| **P13** | **`POST /admin/v1/command`**, typed, explicit target slot, reusing the `cmd_*` internals at `commands.rs:185-206` with the target supplied rather than taken from a connection. | puna M12 |
| **P12** | **`POST /admin/v1/slots/<n>/password`** — rotate one slot on a live room. Without it, rotation costs a pod restart. | puna M12 |
| **P7** | **Track activity** — an `AtomicU64` coarse timestamp in the actor's message handler, surfaced as `activity.last_client_message_at`. | the idle reaper, after M12 |
| **P15** | **The tracker.** Your own plan; noted because puna links to it and needs only the URL shape. | puna M8's link |
| **P5** | **A filtered second listener.** `SERVE_OPTS` has no `--filtered-port`. Puna reserves the port pair as the cluster design requires but deliberately does **not** publish `game-filtered`, because a Service port with no backend is a connection-refused that reads as a puna bug. | whenever |
| **P9** | **Put the build version in the startup line.** `serve.rs:123` prints slots, locations, seed and address but not the build. | trivial |

Critical path for puna: **P10 → (P3/P11 + P14) → P4 → P13.**

### A note on P14, because it is bigger than one flag

There is **no TLS crate in the tree, and none in `Cargo.lock`.** Adding one means choosing a crypto
provider, and pahoa's current posture makes that a real decision rather than a default: a static
musl `scratch` build, `panic = "abort"`, fat LTO, the pure-Rust `zlib-rs` backend chosen
deliberately, and exactly one C dependency (mimalloc).

- `rustls` is the right family — it links no OpenSSL, so the static musl build and the `scratch`
  image survive. `native-tls` would not.
- Its provider is the question. `ring` and `aws-lc-rs` both bring C and assembly. There is house
  precedent for `ring`: `/home/troy/src/Archipelago-lobby/common/Cargo.toml` uses
  `rustls = { features = ["ring"] }`. If keeping the C dependency count at one matters, that is worth
  deciding consciously now rather than discovering it during the build.

Two behaviors puna relies on:

- **One port serves both schemes.** `accept.rs` already sniffs the first byte — it currently answers
  a TLS ClientHello with a raw `handshake_failure` alert so that a `wss://`-first client falls back
  immediately. Extending that sniff to actually terminate TLS is the natural shape, and it is why
  puna publishes a single port.
- **Reload on file change.** cert-manager renews roughly every 60 days and the kubelet updates the
  mounted Secret in place within about a minute. If pahoa reads the cert once at startup, puna would
  have to bounce every running room on a renewal cycle; reloading makes renewal invisible. Existing
  connections keep their negotiated session either way — only new handshakes need the new chain.

The certificate itself is puna's problem, not pahoa's: a dedicated cert for `mw.ionium-dev.us`
(every room shares that hostname and differs only by port), mounted read-only at
`/etc/pahoa/tls/{tls.crt,tls.key}`.

---

## What is NOT verified

Stated plainly so nobody inherits these as facts.

1. **That `--bind=::` works.** Puna passes `::` rather than `0.0.0.0`, because the cluster convention
   requires it — the Services are v6-capable and a v4-only listener answers a v6 connect with an
   instant RST. Pahoa binds via `TcpListener::bind((String, u16))` and defaults to `0.0.0.0`. That
   `"::"` parses **and** accepts v4-mapped connections on this platform has not been tested.
2. **That the accept-path byte sniff extends cleanly to serving both schemes.** The sniff exists and
   the TLS branch is currently a rejection; that terminating TLS there composes well with the
   WebSocket upgrade path and the new HTTP router is an assumption about the code's shape, not a
   measurement.
3. **The acme-dns two-TXT-slot constraint.** Puna's certificate design assumes acme-dns retains only
   the two most recent TXT values per registration, which is why room certs get their own
   registration instead of reusing the gateway's wildcard. That is inferred from a warning in
   `int-k8s/apps/ap-lobby-gateway/manifests/gateway.yaml` — the apex and wildcard "publish their
   challenges at the SAME name… so two TXT records must exist" — rather than read from acme-dns's own
   configuration. It affects puna's manifests, not pahoa's code, but it is the kind of inference worth
   flagging before someone relies on it.
4. **Whether hundreds of Services can share one Cilium LoadBalancer address.** Unrelated to pahoa, but
   it gates the whole deployment and is untested; puna's first milestone is that experiment.

---

## Why these shapes, briefly

The alternatives all look cheaper until you know what ruled them out.

**Environment over argv, for secrets.** Argv is readable inside the container via `ps` and, more to
the point, in `kubectl get pod -o yaml`. Environment values written literally into a pod spec are
*equally* visible there — so the win only materializes because puna sources them from a Secret via
`envFrom`, which leaves nothing but a reference in the object. Files in the room's CephFS directory
were the earlier plan and would also have worked; the environment was chosen because it keeps pahoa's
configuration in one place and needs no path plumbing.

**One port over two.** An unpublished second listener for `/admin/v1/**` would have kept the mutating
surface off the internet entirely, and that was the earlier recommendation. It was rejected on
purpose: it would also block a technical operator from driving the API directly, which is a capability
worth more than the isolation. The consequence is that the bearer token is the only control, which is
why its hardening is called out as non-optional above.

**Typed commands over a raw command line.** Feeding the existing dispatcher a command string looked
cheapest and auto-syncing. But every handler still needs a caller identity supplied from outside —
they are connection-scoped — so most of the saving evaporates, and puna's UI degrades to a text box
with no validation and no slot picker. A typed enum also makes an unknown command a `400` rather than
a confusing text reply. If an escape hatch turns out to be wanted, a raw variant can be added later
alongside the typed set.

**Reload over bounce, for certificates.** Bouncing is genuinely cheap for one room —
`strategy: Recreate` makes it a clean ~10s restart clients reconnect through. It is not cheap across
hundreds of rooms on one renewal cycle, and it would need rate-limiting to avoid a thundering restart.
