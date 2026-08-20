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

`POST /admin/v1/command`, `POST /admin/v1/slots/<n>/password` and the tracker are implemented too —
see below. Every route on the handoff's list now exists.

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

---

## The tracker: `GET /api/tracker` and `GET /api/static_tracker`

Mirrors of the reference WebHost's endpoints of the same names, field for field, verified against a
live `archipelago.gg` document. A tracker page written for the reference works against a pahoa room
with only its base URL changed. `docs/tracker.md` has the shapes and the reasoning.

**Serve the tracker's assets from puna and let its JavaScript fetch the room directly.** That is
cross-origin — a different port alone makes it so — and it works: both endpoints send
`Access-Control-Allow-Origin: *`, exactly as the reference does. Because they are plain `GET`s with
no custom headers they are *simple requests*, so there is no preflight and pahoa needs no `OPTIONS`
route.

Three things that would break it, none of them CORS:

- **Mixed content.** An `https://` puna page cannot fetch an `http://` room at all, whatever the
  headers say. The room must be serving TLS with a **browser-trusted** certificate — cert-manager,
  not a self-signed pair.
- **Adding an `Authorization` header** would make the request non-simple and require a preflight
  pahoa does not answer. The tracker is deliberately public; do not put a token on it.
- **Cookies.** `Access-Control-Allow-Origin: *` is rejected by browsers for credentialed requests.
  Nothing here needs them, and `credentials: "omit"` (the default) is correct.

**Both documents are cached** — 60 seconds for `/api/tracker`, 300 for `/api/static_tracker`, the
same windows the reference memoizes with. Polling faster than that gains nothing and costs a round
trip. The staleness is bounded and identical to what `archipelago.gg` already gives, so a page
cannot tell the difference.

**`activity_timers` and `connection_timers` survive a restart**, at whole-second resolution. That
matters because an async routinely outlives the pod serving it: a room that came back reporting
"never connected" for every slot would lose the one signal that tells an abandoned slot from an
active one, which is exactly what an organizer reads a tracker for. A slot that has genuinely never
acted still reports `null`, never a zero that would render as 1970.

They ride in the save, so this is another reason the drain-time budget in the first section of this
document matters — a room SIGKILLed past its grace period loses them back to the last completed
save along with everything else.

**Where this is going.** The polling API is for compatibility. The direction is a WebSocket path a
tracker connects to, receiving the current state once and then deltas for as long as it stays
connected — which is the thing pahoa can do and a database-backed WebHost structurally cannot,
since the room knows a check landed in the tick it processed it. Recorded in `docs/tracker.md`.
Puna should treat the polling endpoints as the contract now and the fallback later.

---

## Round two: P16, P17 and the tracker gate

All three decided, all three implemented. One is a **behavior change puna must act on**; the others
are safe.

### P17 — per-slot passwords now fail **closed**

`PAHOA_SLOT_PASSWORDS` being set is what puts a room in per-slot mode. Once it is in force, a slot
**missing from the map is refused**, not admitted. The map says who holds a key, not who needs one.

This is the change to act on, and it has a sharp edge:

- **`{}` is now a locked room, not an unconfigured one.** Rendering the variable as an empty object
  — which an orchestrator might do while a slot list is still being assembled — produces a room
  nobody can join. Do not emit `PAHOA_SLOT_PASSWORDS` at all unless per-slot mode is intended.
- **A map that is merely incomplete now locks slots out rather than leaving one open.** That is the
  point: the failure that motivated this was a map built from a player-filtered list, leaving the
  spectator as the single unauthenticated door. It fails loudly at the affected player instead of
  silently at the room.
- **`POST /admin/v1/slots/<n>/password` with `{"password": null}` bars that slot** rather than
  opening it. That is deliberate and useful — one call locks a slot mid-async, no restart, nobody
  else disturbed — but it is the opposite of what the name suggests, so it is worth knowing before
  reaching for it.
- **Rotation requires the mode to already be in force.** With no `PAHOA_SLOT_PASSWORDS`, the route
  returns `404`; there is no per-slot mode to rotate within.

Deliberately *not* the startup cross-check the handoff leaned toward. Coupling the secret to the
seed at startup would make the two undeployable independently, and the fail-closed connect check
already removes the open door — which was the actual concern.

### P16 — two accessors, and the rule written down

`docs/slots.md` is the answer to "if that split is right, write it down". Roster questions —
`/api/v1/room`, `/admin/v1/status` — are now **`connectable_slots`**: players and spectators, groups
excluded, matching `WebHostLib/upload.py`. Progress questions stay players-only.

**A connected spectator now appears in `/admin/v1/status` and `/api/v1/room`**, which it did not
before. If puna renders either directly, expect one more row per spectator.

This also fixed a bug of ours: `/api/tracker` was emitting spectators and groups in every
per-player array. The reference walks `get_all_players()` for those and `get_all_slots()` for hints
alone, and pahoa now does the same. Invisible on a seed with neither, which is why it survived
review — no zip we had contained a group.

### The tracker is gated when an admin token exists

**Not just for race seeds.** The reasoning that decided it: an open tracker on a public port lets an
anonymous port scan read the participant list out of every room, which turns a port range into an
index from "whose game is this" to an address.

The threat is identification rather than name disclosure. Running without a room password is the
common case — the reference defaults to it and most groups leave it — and those rooms are protected
in practice only by the fact that a scanner cannot tell which port is the interesting one. An open
tracker removes that: sweep the range, read the slot lists, find the room containing a streamer or
any other high-visibility player, and griefing it costs one connection. So the gate preserves a
property unpassworded rooms already depend on, and gating on `race_mode` would have left exactly
the population that relies on it exposed.

The rule is about deployment, not seed:

- **A token configured** — every puna room — and the tracker requires it, exactly like
  `/admin/v1/**`. Puna proxies server-side and holds the token, so **nothing changes for puna**.
- **No token** — a standalone pahoa — and it stays open and browser-fetchable, which is what the
  CORS headers are for.
- **`--open-tracker`** restores the open behavior alongside an admin API, for an operator who wants
  a public tracker.

The consequence worth stating: a gated tracker is **not** fetchable from a page, because sending
`Authorization` makes the request non-simple and needs a preflight pahoa does not answer. Puna's
server-side proxy is therefore the supported path for an orchestrated room, not merely the
preferred one — which matches what puna already built for its own reasons.

`race_mode` remains parsed and unused by this decision. If a race wants something stronger than the
gate — a reduced document, say — that is a separate question and pahoa has the flag it would need.

## Round three: P18 is declined — there will be no live password setter

**Position 1.** Pahoa will not implement `/option password` or any equivalent, and this is settled
rather than deferred. Nothing is needed from puna: no `PAHOA_MANAGED_BY`, no identity signal of any
kind. The analysis that framed the question was right and the answer falls out of it.

The reason is simpler than the orchestrated/hand-run distinction the question turned on. That
distinction is real but it is not the deciding one — **a live password change is wrong in every
deployment, not just under an orchestrator.** Pahoa persists no password, so the setter would revert
at the next restart whoever ran it. Under puna it also disagrees with the console in the meantime;
hand-run it merely reverts silently. Those are two severities of the same defect, and a command whose
best case is "correct until something restarts" is not one worth shipping. So there was never a
signal to look for, which is why the `password_from_env` dead end could not be worked around.

Rotation belongs to whatever owns the configuration. Under puna that is the Secret and a room
restart, which puna already does for every `slot_auth` transition anyway. Standalone, it is the
operator restarting the process — which is a fair thing to ask of someone who started it from a
shell in the first place.

**This covers `server_password` too**, by the same argument — it is the third non-persisted secret,
so `/option server_password` would revert on restart identically.

## ⚠ WebSocket keepalives — new, on by default, and puna should check its idle timeouts

**pahoa now pings every connection every 20 seconds and drops one that has not answered within 20.**
`--ping-interval` and `--ping-timeout` override, `0` disables either. This matches the reference,
which inherits both numbers from `websockets`.

**This was missing entirely and it is the third member of the P21 family.** P21 was "the server
forgot a client that thought it was connected". Its mirror is "the client is gone and the server
still thinks it is connected", and nothing detected that: pahoa answered pings but never sent one,
and discarded pongs without recording them.

The part that makes it urgent rather than tidy: **Archipelago's own clients disable their pings**
(`CommonClient.py:872` passes `ping_interval=None`), so keepalive is the server's job by design. A
pahoa room therefore had *no traffic in either direction* on an idle connection. Confirmed in the
wild on Troy's own machine — a browser client that pings survived, a custom client that did not was
dropped, same host, same path. Something on that path reaps idle flows.

What puna should do:

- **Check the ingress/LB idle timeout for the room ports.** 20s of cadence beats a 60s reaper with
  3× margin, but if anything on the path is more aggressive than ~40s, lower `--ping-interval`
  rather than discovering it as mysterious disconnects.
- **Expect `lag_disconnects` to stay put and connection counts to become honest.** A dead peer now
  leaves within `interval + timeout` (40s worst case) instead of holding its slot forever, so
  `clients_connected` and the tracker's connected states stop drifting upward over a long async.
- Nothing to configure otherwise; the defaults are the reference's.

`SO_KEEPALIVE` is deliberately not set, and the reason is worth recording because it is the obvious
future instinct: it would shed connections wedged by a bug in pahoa itself, which sounds like
robustness and is really a way to ensure nobody ever reports the bug.

## P21 — fixed: a close no longer depends on the queue it is closing

**The diagnosis was right and the mechanism was worse than described**, which is worth setting out
because it changes what "fixed" had to mean.

The report identified a close `try_send` onto a queue that had overflowed. That is real, but it is
not the common case. The usual way to lag is to exhaust the **byte budget** while the writer sits in
a `write_all` against a peer that has stopped reading. The queue then has room, so the ordered close
is *accepted* — and waits forever behind frames that will never be written. **A queue that accepts a
close is not a queue that delivers one**, so every path that queued the close succeeded while nothing
reached the socket.

There was a second half, too, and it was the reason the socket stayed open at all: **the writer
finishing did not end the connection.** The reader owns teardown, and it sat in `read_buf` waiting on
a peer that had been told to go away — or never could be. Both halves must drop for a socket to
close, so even a successfully written close frame left the connection hanging when the client was not
reading.

Three changes, all in `crates/pahoa-net`:

- A close signal **separate from the outbound queue**, capacity one, straight to the writer.
- `mark_lagged` uses it **unconditionally**, not just when the queue is full — a lagged connection's
  writer is behind by definition. A kick still prefers the ordered path so the "you were kicked"
  message reaches the player, and falls back.
- The writer races that signal against its own `write_all`, so a wedged write can be abandoned, and
  **the reader now ends the connection when the writer finishes**.

Your note about the counter is right and now says so in its own help text: `lag_disconnects` counts
the *decision*. With this fix decision and effect coincide, but the metric still measures intent.

## P22 — fixed: announcements are `ServerChat` and carry the prefix

Verified on the wire, an admin `say` of "Meow?" now arrives as:

```json
{"cmd":"PrintJSON","data":[{"text":"[Server]: Meow?"}],"type":"ServerChat","message":"Meow?"}
```

Both halves as the reference sends them, including the unprefixed original in `message` so a client
may render either. **Puna needs to change nothing** — the API is unchanged, only what reaches players.

The prefix is applied inside the room rather than by callers, for the reason the report gave: the
admin API has more than one caller, and a caller that forgot it would send a message that
impersonates a player. `!admin`'s own bare-line announcements now go through the same helper, so the
two cannot drift.

## ⚠ P19 — `--snapshot` is gone from `SERVE_OPTS`

**This is the notice P19 asked for. Re-transcribe `PAHOA_SERVE_OPTS` before deploying a pahoa build
newer than this note.** The parser now *rejects* `--snapshot`, so a puna image that still emits it
gets `exit 1` on every room at once — the same failure that started this, in mirror image.

The timing is as P19 described: puna's conditional means it emits no `--snapshot` today, so this
build works against the current puna image unchanged. The trap P19 named is real and worth repeating
— after this, a stray `datapackage.json` on the shared volume makes puna emit the flag again, into a
parser that refuses it, and it looks like adding the right file. Deleting the snapshot machinery and
the `/shared` mount in the same commit as the re-transcription closes it.

### What replaced it

`hint_blacklist` is compiled into the binary — [`crates/pahoa-multidata/src/hint_blacklist.rs`](crates/pahoa-multidata/src/hint_blacklist.rs).
Two entries, matching the two worlds in the reference tree that set one:

```
A Link to the Past               -> ["Triforce"]
Castlevania - Circle of the Moon -> ["Battle Arena: End reward"]
```

Everything else a room needs — item and location names, ids, name groups, checksums — was always in
the seed, so nothing else was ever coming from that file. A game with no entry hints everything,
which is exactly what the reference gives a world that sets none: **absence means "hints everything",
not "unknown"**, so there is no warning and nothing to configure.

`tools/export-datapackage.py` now regenerates that table from an Archipelago checkout rather than
emitting JSON. It refuses to run when any apworld failed to import, because its output *deletes*
entries and an incomplete registry would silently stop `!hint` refusing a name. Worth knowing that a
bare Archipelago venv fails that check — 33 worlds here — so the current entries were established by
reading the source instead.

### One consequence, stated because it is a real narrowing

A seed whose data package WebHost **stripped** on upload (`WebHostLib/upload.py:56-78` replaces it
with `{version, checksum}`) previously fell back to the snapshot for its names. There is no fallback
now: that game is reported unresolved at startup and its names render as `Unknown item (ID:n)`. The
room still hosts — refusing to start over cosmetic names would be worse — but the chat is ugly.

This does not affect puna, and it is worth knowing why rather than taking it on faith: puna ingests
generation zips directly, and a freshly generated `.archipelago` carries a full package. Verified
across all 15 real fixtures — every game resolves from the multidata alone, zero unresolved. It would
only bite a seed that had been round-tripped through a WebHost.

## P20 — fixed: a fatal error is now a JSON event

`report()` routes through the subscriber once one exists, so the case P20 showed — a room dying on a
missing file after the banner — now emits as an `ERROR` event rather than a bare prose line:

```json
{"timestamp":"...","level":"ERROR","message":"/nonexistent.archipelago: No such file or directory (os error 2)"}
```

Stdout stays empty and **every stderr line parses**, which puna's checklist can now assert without
losing the diagnosis. The text is the event's `message` rather than a field, so a viewer keyed on
`message` shows it without knowing anything about pahoa.

The split P20 asked about is on whether a subscriber has been installed, and it is drawn where P20
suggested: failures *before* `init_logging` still use `eprintln!`, because a `--log-format` value
that failed to parse cannot be reported in the format it names. That is the only category, it is
bounded by construction, and both halves have a test — so "every line after startup is JSON" is the
checkable claim rather than "most lines are".

## The room journal: `--journal`, and puna will want it on every room

**New flag, off by default, needs `--save-dir`.** It appends the room's history to `history.jsonl` in
the save directory, one JSON line per event, continuing across restarts. This is the organizer-facing
record, and puna serving it is a file read from a directory it already owns exclusively — no query
language, no cross-room surface.

Every line has a `type`, and **a reader must dispatch on it and ignore what it does not recognize**,
because more types will be added:

| `type` | when |
|---|---|
| `check` | a location became checked, including via release and collect |
| `cheat` | `!getitem` conjured an item — the one item movement no `check` accounts for |
| `hints` | hints granted, with `cost`, `points_before` and `points_after` |
| `chat` | anything said in the room, `!admin` lines already masked |
| `deathlink` | a `Bounce` tagged DeathLink, with `cause` and `source` |
| `options` | at room start and after any change: every option plus `password_mode` |
| `option_changed` | one `!admin /option` |
| `slot_password_changed` | the admin API set or cleared one — `slot` and `set`, never a value |
| `gap` | the writer had to drop records |

**No password is ever written**, and that is enforced on both paths rather than by convention: `chat`
is built from the text the room *broadcast*, which `cmd_admin` has already masked, and the option
records carry modes and booleans (`password_mode`, `server_password_set`, `set`) rather than values.
Verified against a live room — `!admin login hunter2` and `!admin /option server_password topsecret`
both appear masked, and neither secret occurs anywhere in the file.

**Deliberately not in the log stream.** Checks do not reach stderr at any level, so this changes
nothing about what puna ships to Loki. The reasoning is access rather than durability: Loki has no
label-level authorization, so "this organizer reads this room and nothing else" would need room logs
routed to their own tenant; retention is a platform setting an async room can outlive; and a
restarted room is a new pod, so reassembling one room's history from pod logs needs a stable label
promoted through the shipper. A file in the room's own directory needs none of that. Full reasoning
and the format in [`docs/journal.md`](docs/journal.md).

Three things puna should know:

- **Size it.** ~264 bytes per check, so a full playthrough is 6 MB for a 96-slot seed and ~90 MB for
  a 2000-slot one. It grows monotonically and is never pruned by pahoa — if a room directory has a
  quota, this is the file that will find it.
- **It is safe to read while the room is running.** Append-only, line-oriented, flushed every 1024
  records and on the save timer. A reader that tails it or reads it whole will see complete lines; a
  crash can lose the tail, which is the same bargain the save file makes.
- **A gap announces itself.** If a disk stalls badly enough to fill the buffer, the writer drops
  rather than blocking — because the alternative is a stalled disk stopping a live multiworld — and
  writes `{"type":"gap","dropped":n}` into the journal at that point. Anything rendering the history
  should show that line rather than skip it, since it is the only evidence the record is incomplete.

The cost to the room is 0.9% of a mass release: the actor queues `Copy` records and a thread does the
name resolution and JSON. Doing it inline would have cost 286% of that release, which is why the
threading is not incidental.

## Logging: JSON, a startup banner, and reference-level verbosity

**Set `--log-format json` on every puna room.** Default is `text`, for the standalone case where a
person runs a room from a terminal and reads it themselves. JSON emits one object per line on stderr
with every field as a key, so `slot`, `team`, `option` and the rest are queryable rather than
something to regex out of a message.

### ⚠ Constraint 7 changes under `--log-format json` — puna must act on this

**Under `--log-format json` there is no stdout startup line at all.** stdout is silent; the
announcement is a `serving` event on stderr with the same facts as fields:

```json
{"level":"INFO","message":"serving","slots":96,"locations":23404,
 "seed_name":"14318265276849580066","addr":"0.0.0.0:38281",
 "outbound_budget_bytes":67108864,"version":"0.1.0","build_rev":"8073194+"}
```

**Anything matching the text startup line must switch to matching `message == "serving"` when it sets
that flag.** Under `--log-format text` nothing changes: the stdout line is exactly as it was, so a
hand-run room and any existing tooling are unaffected.

The reasoning, since this reverses a constraint rather than merely extending it. A container merges
stdout and stderr into one pod log, so a plain-text line inside a JSON stream is one unparseable
entry per room, forever. The dedicated stdout channel existed *because* logs were prose — it was the
only way to make "the room came up" parseable when everything around it was not. Once the log is
structured that reason is gone, and keeping the line would mean carrying its cost with none of its
benefit. Emitting both was the first answer here and was worse: two records of one event, which
anything counting room starts has to know to de-duplicate.

The `serving` event is deliberately self-contained — `version` and `build_rev` repeat what the banner
already said — so matching that one event answers "which build came up, serving what, where" without
correlating two records.

One consequence worth having: a shipper can now be configured to **reject** non-JSON lines, which is
a useful thing to be strict about. Verified end to end — a full room lifecycle, stdout and stderr
merged as the kubelet would merge them, parses as 19 objects with nothing left over.

**The first event is a banner**, which is what makes a room traceable after it is gone:

```json
{"level":"INFO","message":"Pahoa-0.1.0-8073194+ starting","argv":"…","os":"linux","arch":"x86_64",
 "pid":1,"host":"room-abc","pod":"room-abc","namespace":"pahoa","node":"node7",
 "worker_threads":4,"cpu_quota":4,"host_cpus":64,"memory_limit_bytes":2147483648}
```

Three things puna should act on here:

- **Pass the downward API in.** `pod`, `namespace` and `node` come from `POD_NAME`, `POD_NAMESPACE`
  and `NODE_NAME`, which Kubernetes does not set on its own. Without them the banner still works and
  those keys are simply absent — fields with no value are omitted rather than reported empty, so
  `cpu_quota` missing means *no cgroup cap*, not zero.
- **`worker_threads`, `cpu_quota`, `host_cpus` and `memory_limit_bytes` are on one line on purpose.**
  They are the evidence for constraints 5 and 6. A room reporting `cpu_quota` absent on a 64-core
  node is one whose `limits.cpu` did not apply, and that is now visible at startup instead of after
  a memory incident.
- **`argv` has every password replaced with `***`**, matched on the flag name rather than a fixed
  list, so this holds for a password option pahoa adds later. Nothing in the log carries a secret —
  including `/options` output, which prints the real `server_password` to the administrator who asked
  and is deliberately not logged.

**Verbosity now matches the reference.** At `info`: lifecycle, chat, single-line command replies,
refused connections with their reasons, `!admin` option changes, and save failures at `error`.
Multi-line command output is not logged — `!missing` answers with hundreds of lines and the reference
omits it too. Per-connection and per-message detail is `debug`, which is where the reference puts its
`--log_network` output. Item sends do not log at any level, so a mass release does not flood anything.

**The image needs a build arg.** `.dockerignore` excludes `.git`, so `build.rs` cannot read the
revision in a container build and takes `PAHOA_BUILD_REV` instead. The CI image job now passes
`--build-arg PAHOA_BUILD_REV=$CI_COMMIT_SHORT_SHA`. **If puna ever builds a pahoa image itself, it
must pass the same thing** — omitting it is not a build failure, it just stamps `unknown` and quietly
costs the ability to tie a running room to a commit.

## Round three: `!admin` is implemented, and the rule that shaped it

The exclusion above is narrower than "no live setters", and the rule behind it is worth puna having
because it predicts what pahoa will and will not gain: **a setter is honest exactly where the save is
authoritative.** The gameplay options are — `save::encode_options` persists every one of them and
`Room::restore` takes them from the snapshot — so setting them live is truthful and they now have a
setter. The passwords are not, deliberately, which is the whole of why they do not. Two conclusions
from one rule rather than a special case, and it is recorded on `cmd_admin` in
[`room/commands.rs`](crates/pahoa-room/src/room/commands.rs) and in
[`room/server_commands.rs`](crates/pahoa-room/src/room/server_commands.rs), at the point someone would
add the thing it forbids.

### What shipped

`!admin login <server_password>` opens a session; `!admin logout` or a disconnect ends it; one
administrator at a time, replacing rather than accumulating, as the reference does. Then:

- **`/option <name> <value>`** — `hint_cost`, `location_check_points`, `release_mode`,
  `collect_mode`, `remaining_mode`, `countdown_mode`, `item_cheat`, `compatibility`. Persisted at the
  next save; survives restart.
- **`/options`** — the current values.
- **`/help`**.
- **Anything not starting with `/`** is announced to the room as `[Server]: …`, which is how an
  organizer says something that does not read as coming from their own slot.

`/option password` and `/option server_password` are refused **by name**, with a message saying they
would revert at the next restart and to set them where the room is configured — not reported as
unknown options, because they are recognized and declined and those are different facts.

Deliberately **not** implemented: `/release`, `/collect`, `/send`, `/kick`. They exist on the admin
API, which authenticates with a bearer token over TLS rather than a password typed into chat, and
does not put the operation in the room's chat log. `/help` says so, so an administrator who reaches
for one learns where it went instead of concluding the shell is broken.

### Two things puna should act on

**1. On gameplay options the room is the source of truth, not puna.** This is the opposite of the
password contract. Whatever puna passes as flags is an *initial* value; after the first save the
room's own copy wins, including any live `/option` an organizer ran. A room up for a week may
legitimately disagree with its own manifest, and that is correct rather than drift. This was already
true before `!admin`, purely from the restore path — `!admin` only makes it reachable.

The concrete case to expect: an organizer runs `!admin /option hint_cost 15`, puna later redeploys
that room for an unrelated reason with `--hint-cost 10` still in the pod spec, and the room comes up
at 15. **That is correct** — the alternative silently discards what the organizer did — but it used
to happen with no trace at all, which is the same silence that hid the password-persistence bug for
as long as it did. It is now a startup `WARN` naming the flag, both values, and which won:

```
WARN pahoa::serve: --hint-cost asked for 99, but the restored save says 15 and wins; room options
live in the save once one exists, and are changed with !admin /option or by starting from an empty
--save-dir
```

Only flags actually passed are compared, so a room puna starts without option flags stays quiet.
Worth surfacing in puna's room log view rather than filtering out: it is the one signal that a
manifest and a running room have diverged, and it is the answer to "I changed the setting and
nothing happened."

**2. `/admin/v1/status` now has an `options` block, and that is what to render.**

```json
"options": {
  "hint_cost": 10, "location_check_points": 1,
  "release_mode": "auto", "collect_mode": "auto",
  "remaining_mode": "goal", "countdown_mode": "enabled",
  "item_cheat": true, "compatibility": 2
}
```

Modes are the word rather than the bitmask, since this document is read by people as well as by
puna. The three passwords are absent by design; `/api/v1/room`'s `password_required` already answers
what a status reader legitimately needs without disclosing a secret.

Before this there was no way to read a room's effective `hint_cost` without speaking the game
protocol, so anything rendering it from puna's own configuration was showing a value that might be
false. Clients were never affected — `RoomInfo` has always carried these at connect, and `/option`
now pushes `RoomUpdate` deltas exactly as the reference does: one room-wide broadcast for the
permission modes, and a **per-slot** update carrying recomputed `hint_points` for `hint_cost` and
`location_check_points`, because hint cost is a percentage of a slot's own location count and one
shared number would be wrong for every slot but one.

**`POST /admin/v1/slots/<n>/password` is unaffected and stays live**, exactly as the question asked.
The line is the one puna drew: the room-wide password and the per-slot *mode* are configuration and
move at restart; individual per-slot values are operational and move immediately.

That endpoint stays an *HTTP* one, though, and the distinction is worth keeping if `!admin` is ever
built out. It is bearer-authenticated over TLS to a single caller. Reaching the same mutation through
`!admin` would put a password in a chat command — echoed to the room, masked but present in the
sender's own client, and through the room's inbound path. Same operation, materially worse credential
handling, so "the per-slot setter stays live" means over HTTP and not over any channel that asks.

One consequence for puna's console: pahoa now has no path at all by which a room-wide password
changes while the room is up. If the console offers such a control it must be a puna-side edit plus a
restart, never a call into pahoa, because there is no endpoint to call and there will not be one.
