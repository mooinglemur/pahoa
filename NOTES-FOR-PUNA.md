# Notes: `pahoa` → `puna`

Things found while implementing `HANDOFF.md` that puna needs to know, accumulating here to be
handed back in one piece rather than a message at a time. The counterpart to `HANDOFF.md`, in the
other direction.

---

## Shutdown drain time, against the pod's grace period

**Set `terminationGracePeriodSeconds` explicitly on room Deployments. Do not inherit the 30-second
default.**

Since P1 landed, a room handles SIGTERM and runs a final save on the way out — that is the whole
point of the change, and it is what makes a rollout or a node drain lose nothing. But the budget
that save runs in is bounded by pahoa and the budget the *pod* gets is bounded by Kubernetes, and
the two are close enough together to collide.

What happens between SIGTERM and exit, worst case:

| Step | Bound | Where |
|---|---|---|
| Drain the actor mailbox down to the `Shutdown` message | actor throughput over ≤ 8192 queued messages | `server.rs:61` |
| Wait out a save already in flight, so the newest snapshot lands last | `shutdown_timeout`, **10 s** | `actor.rs:248-250` |
| Encode, write, fsync the final save | `shutdown_timeout` again, **10 s** | `actor.rs:253-262` |

So roughly **20 seconds of disk budget** plus drain, against a 30-second default. It fits, with
very little margin — and the case that consumes the whole thing is exactly the case the save
matters most in: a CephFS MDS failover, where reads and writes *block* rather than erroring and no
userspace timeout can cut them short.

If the grace period expires first, the kubelet SIGKILLs. The flock releases either way (the kernel
closes the file), so the next pod still starts — but the room falls back to its last completed
save, losing up to `--save-interval` of play. That is precisely the loss P1 existed to remove, so
inheriting the default quietly undoes the fix on the worst day.

**Recommendation: `terminationGracePeriodSeconds: 45`.** Twenty seconds of flush, plus drain, plus
margin, and still well inside any reasonable rollout budget.

Two related facts:

- **`shutdown_timeout` is not tunable from outside pahoa today.** It comes from
  `SaveConfig::default()` (`actor.rs:162`) and `serve.rs` does not override it, so there is no flag
  puna can set. If the 45-second recommendation turns out to be wrong in the cluster, the fix is a
  new pahoa flag, not a puna manifest change — tell us rather than working around it.
- **A room that overruns says so.** `actor.rs:257-261` logs `the final save did not finish in time;
  state since the last completed save is lost` at `warn`. Since P2 there is a subscriber, so that
  line actually reaches container logs now. It is worth alerting on: it is the signal that the
  grace period is too short for this cluster's filesystem, and it names the exact damage.

---

## `--bind=::` is verified now, with one dependency

`HANDOFF.md`'s first unverified claim. It works: `--bind ::` parses, binds `[::]`, and accepts
v4-mapped IPv4 connections. There is a regression test —
`the_v6_wildcard_accepts_a_v4_mapped_connection` in `crates/pahoa-net/tests/tls.rs` — which skips
rather than fails where IPv6 is unavailable, so it will not produce a false green.

The dependency worth knowing: v4-mapped acceptance is the kernel's `net.ipv6.bindv6only=0`, which is
the Linux default but *is* a sysctl. If a node or pod sets it to `1`, a room bound to `::` stops
answering IPv4 entirely and the symptom is a connection refused that looks like a pahoa bug. Worth
one line in a cluster preflight check rather than a discovery during an async.

---

## TLS is terminated in pahoa, and the certificate is hot-reloaded

`--tls-cert` and `--tls-key`, both plain paths (not secret, so they stay argv). Mount them wherever;
`/etc/pahoa/tls/{tls.crt,tls.key}` as the handoff proposed is fine and nothing in pahoa depends on
that path.

What puna should rely on, and what it should not:

- **One port, both schemes, no proxy.** Verified against the shipped `scratch` image: TLS 1.3
  handshake, `wss://` upgrade, ALPN negotiating `http/1.1`.
- **Plaintext is refused by default** once a certificate is configured — `426 Upgrade Required` with
  an `Upgrade: TLS/1.3, HTTP/1.1` header. `--allow-plaintext` opts back in. Do not set it in the
  cluster; it exists for local debugging. This is deliberately stricter than the handoff's "one port
  serves both schemes" phrasing, because the admin token is mutating and internet-reachable.
- **Renewals need no restart.** Both files are polled every 30 seconds and re-read on change, so a
  cert-manager cycle is invisible and puna never has to bounce rooms for one. Established
  connections keep their negotiated session; only new handshakes see the new chain.
- **A bad pair does not take a room down.** A half-written file, or a new chain next to the old key,
  is rejected and the previous certificate keeps serving, with a `warn` naming the file. Do not
  design a health check that expects a restart on a bad certificate — there will not be one.
- **A bad pair at *startup* is fatal**, on purpose: the certificate is read before the listener
  binds, so a misconfigured room fails its probe rather than binding and failing every handshake.

One thing puna does need to get right: **the cert must be readable by the room's user**. The image
is `FROM scratch` with no `/etc/passwd`, so if pod security forces a `runAsUser`, the Secret's
`defaultMode` has to permit that uid. The failure is a startup error naming the path, which is at
least legible.

---

## P5: the filtered feed is a second port, and it is **implemented — publish `game-filtered`**

`--filtered-port <n>` opens it. This supersedes the standing advice to reserve the pair and not
publish it: there is a backend now, so the Service port should go live. The design is in
[`docs/scoped-feed.md`](docs/scoped-feed.md); the short version for puna is:

- The scoped port serves a client only the messages relevant to its own slot, dropping the
  `PrintJSON` firehose — `ItemSend` between other slots, join/part spam, other players' hints.
- **It is a port precisely because it needs no client changes.** The clients that most need it
  cannot handle the full feed *and* have no way to select a tag or a URL path, so the server applies
  the policy on their behalf based on which port they arrived on. That is why a path on the single
  port was the wrong shape, and it is a better reason than the network-policy one recorded here
  previously.
- **Both listeners are the same server.** When both are active the HTTP surface — `/healthz`,
  `/api/v1/room`, `/admin/v1/**`, the tracker — is served identically on both, and TLS terminates
  identically on both. Only the WebSocket feed differs. So puna may probe or drive the admin API
  through either port.
- They share one `TlsAcceptor`, one certificate and one reload timer, so a renewal cannot land on
  one port and not the other. `--allow-plaintext` and the `426` refusal apply to both.

What puna needs to do differently:

- **Pass `--filtered-port` and publish `game-filtered`.** It must differ from `--port`; pahoa
  refuses to start otherwise, and refuses to start if either port is already in use rather than
  coming up serving half of what it advertised.
- **Both ports need the same treatment** — the same Service, the same TLS, the same
  `readinessProbe` target if you probe either. `/healthz` answers on both.
- **Advertise both to players.** The scoped port is the one to hand to someone whose client chokes
  on a large multiworld; the full port stays the default. `advertised_filtered_port` in puna's
  schema is what this fills in.

Measured on a 75-slot seed: a client watching one slot while a neighbour released its entire world
saw **4** item messages on the scoped port against **130** on the full one, with chat, the
countdown and the release announcement arriving intact on both.

---

## Secrets: the contract holds, with one addition and one gap

`PAHOA_PASSWORD`, `PAHOA_SERVER_PASSWORD` and `PAHOA_SLOT_PASSWORDS` are implemented exactly as
`HANDOFF.md` specifies, including the flat quoted-key JSON shape and the startup error when both
password modes are set. Slots absent from the object have no password. `PAHOA_SERVER_PASSWORD` is
orthogonal and coexists with either mode.

**`PAHOA_ADMIN_TOKEN` is implemented now** — see the admin API section below for what it unlocks
and what it demands.

**One addition worth knowing about: precedence is environment, then seed, then argv.** The middle
term is new. `--use-embedded-options` applies a seed's own `server_options` over the command line,
and a seed can carry `password` and `server_password` — so without this rule, a seed generated with
a password would have silently overridden the one puna configured, and rotation would have appeared
to work and then reverted on the next restart. The environment now wins over the seed for secrets
only; every non-secret option still follows the seed, as the reference does. If a seed's password
is ignored for this reason, the room says so at `warn`.

**And a bug that was live and is now fixed, which affects what puna can promise about rotation:**
passwords used to be persisted into `room.save`, and `Room::restore` assigned the decoded options
wholesale — so the value on disk beat the configured one on every restart. Rotating a password
would have appeared to work and then reverted. They are no longer written at all, and restore
explicitly carries the configured secrets across. There are regression tests for both directions,
including that a room restarted *without* a password does not have one restored from disk.

The save format did **not** get a version bump for this, on the grounds that no live saves exist. A
`room.save` written before this change will fail to decode. If any pre-existing save directories are
lying around from experiments, delete them.

---

## The HTTP surface is live: probes, the room page, and the admin API

All on the room port, all over the same TLS. Five routes so far, matching `HANDOFF.md`'s contract:

| route | auth | notes for puna |
|---|---|---|
| `GET /healthz` | none | The readiness probe. `200` means *this room is really serving* |
| `GET /api/v1/room` | none | The room page's data. Verified to carry no secrets |
| `GET /admin/v1/status` | bearer | The document from `HANDOFF.md:116-127` |
| `GET /admin/v1/metrics` | bearer | Prometheus text exposition |
| `POST /admin/v1/shutdown` | bearer | `202`, then quiesce, save, exit 0 |

`POST /admin/v1/command` and `POST /admin/v1/slots/<n>/password` are implemented too — see below.
`/tracker/…` is still `404`.

**Use `GET /healthz` as the readiness probe, not a TCP check.** The listener binds only after the
save is restored, so either works today, but the HTTP probe stays correct if that ever changes.

Four things that constrain puna's manifests:

- **`PAHOA_ADMIN_TOKEN` must be at least 32 bytes, and pahoa refuses to start below that.** The
  error names the length it got, never the token. Generate it — the surface is mutating and
  internet-reachable, and the token is the only control on it.
- **With no token set the admin routes return `404`, not `401`.** Absent rather than locked, so a
  Secret that failed to render fails closed and looks like an old image rather than a locked door.
  If puna probes `/admin/v1/status` and sees `404`, the token did not arrive.
- **Authentication failures are rate-limited to 10 per minute per room, and the cutoff applies to
  the correct token too** for the rest of that window — otherwise the limit would be an oracle. A
  reconciler that retries a failing call in a tight loop can therefore lock *itself* out for a
  minute. Back off on `429`; it carries `Retry-After`.
- **`clients_connected` counts sockets, not players.** A player commonly holds three — game client,
  text client, tracker. The per-slot `connections` field in `/admin/v1/status` is the same count
  scoped to one slot, and `connected` is just `connections > 0`. An idle reaper should read
  `activity.idle_seconds`, not the client count.

Two shapes worth knowing before coding against the JSON:

- **`save` is `null`** for a room started without `--save-dir`, rather than a block full of zeros.
  A room that keeps nothing is a real state and should not look like a room that has never saved.
- **`activity.last_client_message_at` and `idle_seconds` are `null`** until a client has said
  something. Null means "no data", never "idle for zero seconds".

**Shutdown ordering, since puna will drive it:** the `202` is written *before* the room quiesces,
because quiescing closes every connection including the one asking. After answering, the room takes
exactly the SIGTERM path — stop accepting, tell clients the room is closing, flush the final save,
then a brief linger so those close frames reach the wire before the runtime drops. The drain-time
budget from the first section of this document applies unchanged.

---

## Commands and password rotation

`POST /admin/v1/command` takes the tagged set from `HANDOFF.md` verbatim — `status`, `say`,
`countdown`, `release`, `collect`, `send_item`, `hint`, `kick` — and answers
`{"ok", "output", "affected_slots"}` in every case. `output` is pahoa's own phrasing, meant to be
rendered verbatim in a console pane.

**The status-code split matters for puna's error handling.** A *malformed* request is a `400`: an
unknown command, a missing field, a field of the wrong type. A command the *room* refused — slot
does not exist, nobody connected to kick, a countdown out of range — is a **`200` carrying
`ok: false`**, because it was understood and answered. Do not treat `ok: false` as a transport
failure to retry; it is an answer, and `output` says why.

Three behaviors worth knowing:

- **An administrator is not bound by the modes that gate players.** `--release-mode disabled` stops
  `!release` and does not stop `{"command":"release"}` — acting for someone who cannot is the point
  of the API. `send_item` likewise works with `--no-item-cheat` set.
- **`hint` has two modes.** `force: true` grants it outright and spends nothing; the default
  (`force` absent or false) charges the slot's own points exactly as `!hint` would, and may grant
  fewer hints than asked or none. `granted` in the output line is the truth, not the request.
- **`kick` is a disconnect, not a ban.** Every connection the slot holds is closed with a clean
  WebSocket 1001 after the operator's reason is delivered as chat, and nothing prevents an immediate
  reconnect. The response says so.

**`POST /admin/v1/slots/<n>/password`** takes `{"password":"…"}` to set one and `{"password":null}`
(or an empty body) to clear it. `404` for a slot the seed does not have. Verified live: a slot
goes from open, to refusing an empty and a wrong password, to accepting the new one, and back to
open when cleared — all without a restart, and without affecting any other slot.

**It survives a restart only because puna's environment stays authoritative.** The rotation changes
the running room, and nothing about a password is persisted. So a rotation done through this route
and *not* mirrored into `PAHOA_SLOT_PASSWORDS` will revert to the environment's value the next time
the pod restarts. That is deliberate — it is what stops a stale on-disk password shadowing the
configured one — but it means **puna must treat its own Secret as the source of truth and use this
route to avoid the bounce, not instead of updating the Secret.**
