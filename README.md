# pahoa
Alternative Archipelago Multiworld MultiServer implementation written in pure Rust.  Designed to be deployed containerized per multiworld. Designed mainly to solve concurrency and performance limitations of the reference implementation.

## Building

```sh
cargo build --release
```

The shipping artifact is a fully static musl binary in a `scratch` image, built
and verified — linkage *and* known-answer tests — by the `Dockerfile`:

```sh
docker build -t pahoa .
```

## Usage

```
pahoa serve <file.archipelago> [options]     Host a multiworld
pahoa inspect <file.archipelago>             Summarize a multidata file
pahoa selftest                               Verify the build against known-answer tests
pahoa --version
```

`pahoa --help` prints the same reference as below. The reference server's
underscored spellings (`--hint_cost`, `--release_mode`, `--disable_item_cheat`,
`--host`, …) are accepted as aliases everywhere, so muscle memory carries over.

### Serving

| option | default | |
|---|---|---|
| `--bind <addr>` | `0.0.0.0` | Listen address |
| `--port <n>` | `38281` | Listen port |
| `--filtered-port <n>` | — | A second port serving the scoped feed |
| `--snapshot <file.json>` | — | Data package snapshot, from `tools/export-datapackage.py` |
| `--save-dir <dir>` | — | Where the room persists itself |
| `--save-interval <secs>` | `60` | Save cadence |
| `--journal` | off | Append a per-check history to `history.jsonl` beside the save |
| `--outbound-budget <MiB>` | derived | Cap on queued outbound data across all clients |
| `--log-level <level>` | `info` | `trace`, `debug`, `info`, `warn`, `error` |
| `--log-format <fmt>` | `text` | `text` for a terminal, `json` for a log aggregator |
| `--tls-cert <file.pem>` | — | Certificate chain; terminates TLS on the room port |
| `--tls-key <file.pem>` | — | Its private key. Both or neither |
| `--allow-plaintext` | off | Keep answering `ws://` after a certificate is set |

```sh
pahoa serve seed.archipelago --port 38281 --save-dir /var/lib/pahoa/room-1
```

### The room journal

`--journal` appends one JSON line per location checked to `history.jsonl` in the
save directory, continuing across restarts — the organizer's record of what
happened in their room and when. It needs `--save-dir`, and it is **not** in the
log stream: nothing about checks goes to stderr.

That split is about access rather than durability. A log aggregator is the right
place for an operator debugging across rooms, but scoping one organizer to one
room's lines is not something Loki enforces, retention is a platform setting an
async room can outlive, and a restarted room is a new pod. A file in the room's
own directory has none of those problems.

The actor pays 0.9% of a mass release to write it — it queues plain numbers and a
thread does the JSON — and the channel drops rather than blocks if a disk stalls,
recording the gap in the journal itself. [docs/journal.md](docs/journal.md) has
the measurements and the format.

### Logging

**Logs go to stderr.** Under `--log-format text`, stdout carries exactly one
line — the startup line, in a fixed shape, so that `pahoa serve … 2>/dev/null`
is a way to read the one thing a machine is meant to parse out of a stream of
prose. Under `json` there is no stdout line: the announcement is a `serving`
event with the same facts as fields, plus `version` and `build_rev`. A container
merges both streams into one log, and a structured log needs no separate channel
to be parseable — so the dedicated stream would cost an unparseable line per room
and buy nothing.

The first event is a banner naming the build, the invocation and the machine:

```
Pahoa-0.1.0-8073194+ starting argv=… os=linux arch=x86_64 pid=1 host=room-abc
  worker_threads=4 cpu_quota=4 host_cpus=64 memory_limit_bytes=2147483648
```

`8073194+` is the source revision, with `+` meaning the tree had uncommitted
changes — `0.1.0` is every build for months and cannot tell two rooms apart. It
comes from git when building from a working tree and from `PAHOA_BUILD_REV`
otherwise, because the container build has no `.git`. **`argv` has every password
value replaced with `***`**, matched on the flag name rather than a fixed list.
`pod`, `namespace` and `node` appear when the downward API supplies them, and
fields that have no value are omitted rather than reported empty — so
`cpu_quota` missing means no cgroup cap, not zero.

Verbosity tracks the reference server. At `info` a room reports its lifecycle
(start, restore, TLS, shutdown), chat and single-line command replies, refused
connections, option changes made through `!admin`, and save failures at `error`.
Multi-line command output is *not* logged — `!missing` can answer with hundreds
of lines, and the reference omits it for the same reason. Per-connection and
per-message tracing is `debug`, matching what the reference puts behind
`--log_network`.

Two things are deliberately never logged: the values `/options` prints, which
include the real `server_password`, and any pre-masked `!admin` line. The
administrative action is still on the record — the masked command is logged as
chat, and an option change gets its own event with the option and new value as
fields.

### The scoped feed

`--filtered-port` opens a second port on which a client receives only what
concerns its own slot: its own item traffic, its own hints, its own joins and
parts. Chat, countdowns and the room-wide milestones — goals, releases,
collects — still arrive in full, because the filter drops firehose and never
anything a human typed.

**It is a port because it needs no client support.** The clients that most need
a quieter feed cannot handle the `PrintJSON` deluge *and* have no way to select
a tag or a URL path, so the server applies the policy on their behalf based on
where they connected. An unmodified client pointed at the scoped port simply
gets a quieter room. On a 75-slot seed, a slot watching a neighbour release its
whole world sees 4 item messages instead of 130.

Both ports are the same server: they terminate the same TLS and serve the same
HTTP surface, and only the WebSocket feed differs. The policy is fixed when the
connection is accepted and a `ConnectUpdate` cannot lower it, which matters
because trackers send those routinely and a policy living in the tags would be
wiped mid-session. [docs/scoped-feed.md](docs/scoped-feed.md) has the design.

### TLS

`--tls-cert` and `--tls-key` terminate TLS on the room port itself, so one port
serves `wss://` and `https://` with no proxy in front. The accept path sniffs
the first byte of each connection, which is what lets both schemes share it:
a TLS ClientHello starts `0x16`, and no HTTP method can, since every one of
those is uppercase ASCII.

Once a certificate is configured, plaintext is **refused** with `426 Upgrade
Required` unless `--allow-plaintext` is given. That default is deliberate: the
admin API is mutating and internet-reachable, and serving its bearer token in
the clear on the same port would undo the point of having it. With no
certificate configured nothing changes — plaintext is served, and a TLS client
gets an immediate `handshake_failure` alert so a client probing `wss://` before
`ws://` falls back at once rather than hanging.

**The certificate is reloaded in place.** Both files are checked every 30
seconds and re-read when either changes, so a renewal needs no restart — which
matters when the alternative is bouncing every running room on one cert-manager
cycle. Polling rather than watching, because the usual publisher of a renewed
certificate is a Kubernetes Secret mount, and the kubelet swaps a symlink
instead of rewriting in place. A pair that does not load — half-written, or a
new chain next to the old key — leaves the previous certificate serving and logs
why, rather than taking the room down over a file it already has a working copy
of. Established connections keep the session they negotiated; only new
handshakes see the new chain.

`rustls` over `ring`, so nothing links against the host and the static musl
build in the `scratch` image is unaffected.

**Logs go to stderr and stdout carries exactly one line** — the startup line
naming slots, locations, seed, address and build. That split is what makes
`pahoa serve … 2>/dev/null` a way to read the line a machine is meant to parse.
With `--log-format json` the split is unnecessary and stdout stays silent; see
[Logging](#logging).
A room stops cleanly on SIGINT or SIGTERM alike, so a container teardown gets
the same final save that Ctrl-C does.

Without `--snapshot`, games resolve from the seed's own embedded data package.
That covers item and location names and ids for every game in the seed, so a
room runs fine on it; what it never carries is each world's hint blacklist, so
`!hint` cannot refuse a non-hintable name. The server warns at startup for the
games affected.

The outbound budget defaults to 288 KiB per slot — three connections each, since
players commonly run a game client, a text client and a tracker — with a 64 MiB
floor, so a 2000-slot room gets 562 MiB and a small one gets 64 MiB rather than a
cap it could never reach. It bounds what may sit queued for clients that have
stopped reading; a connection past its share is dropped rather than buffered,
which is safe because the protocol resyncs on reconnect. The room prints the
figure it derived at startup.

**`--save-dir` is optional and the room says so loudly when it is missing** —
without it nothing survives a restart, which is right for a throwaway room and a
data-loss bug for anything else. The directory is ordinary: one room per
directory, claimed with an exclusive lock held for the life of the process, so a
second `pahoa serve` pointed at it exits rather than silently overwriting. The
save cadence is what bounds how much play an unclean stop can lose; the flush on
shutdown is a nicety, since SIGKILL, node loss and OOM kills all skip it.

### Passwords

Paths and ports are argv; **secrets are read from the environment**, because
argv is readable with `ps` inside the container and in `kubectl get pod -o yaml`
outside it. An environment value written literally into a pod spec is just as
visible, so the win only arrives when an orchestrator sources these from a
Kubernetes Secret with `envFrom`, leaving a reference rather than a value.

| variable | equivalent | |
|---|---|---|
| `PAHOA_PASSWORD` | `--password` | One password for the whole room |
| `PAHOA_SLOT_PASSWORDS` | — | A password per slot, as JSON |
| `PAHOA_SERVER_PASSWORD` | `--server-password` | Enables `!admin login` |

A room runs in one of three modes: passwordless, one room-wide password, or one
per slot. `PAHOA_SLOT_PASSWORDS` is a flat JSON object, and since JSON object
keys are strings the slot number is quoted:

```json
{"1": "quiet-harbor-ledger", "2": "amber-ferry-quartz"}
```

Setting `PAHOA_SLOT_PASSWORDS` at all is what puts a room in per-slot mode, and
the mode **fails closed**: a slot absent from the object is *refused*, not
admitted. The map says who holds a key, not who needs one, so an incomplete map
locks a slot out rather than leaving it the one open door in the room. An empty
object is therefore a locked room, and clearing a slot's password through the
admin API bars that slot rather than opening it — which is a useful thing to be
able to do mid-async. Setting a room-wide *and* a per-slot password is an error
at startup rather than a silent preference for one. See
[docs/slots.md](docs/slots.md) for which slots this covers. `--server-password` is a third, orthogonal thing — it gates `!admin`, not
joining — so it coexists with either mode.

**Precedence is environment, then seed, then flag.** The flags still work,
because pahoa is also a tool someone runs by hand, but a secret arriving that
way is warned about. The seed sits in the middle because `--use-embedded-options`
exists to honor what a seed was generated with — but a password baked in at
generation time is readable by anyone holding the seed, so it cannot override
one the operator configured.

**Passwords are never written to `room.save`.** The environment is authoritative
on every start, which is what makes rotating one survive a restart rather than
reverting to whatever was on disk.

### The HTTP surface

The room port serves HTTP as well as the game, decided per connection by the
same first-byte sniff that separates TLS from plaintext. With `--tls-cert` set
these are `https://`; without it, `http://`.

| route | auth | |
|---|---|---|
| `GET /healthz` | none | `200` once the room is serving |
| `GET /api/v1/room` | none | What a room page shows. No secrets |
| `GET /api/tracker` | see below | The reference WebHost's tracker document |
| `GET /api/static_tracker` | see below | The half that only changes with the seed |
| `GET /admin/v1/status` | bearer | Clients, save state, net counters, per-slot progress, the room's effective options |
| `GET /admin/v1/metrics` | bearer | The same numbers as Prometheus text |
| `POST /admin/v1/command` | bearer | The typed command set below |
| `POST /admin/v1/slots/<n>/password` | bearer | Rotate one slot's password, live |
| `POST /admin/v1/shutdown` | bearer | Quiesce, save, exit 0 |

`/healthz` needs no state to answer: the listener binds only after the save has
been restored, so reaching it at all is the readiness signal.

**The tracker endpoints mirror the reference WebHost's** field for field, so a
tracker page written against `archipelago.gg` works against a pahoa room with
only its base URL changed — down to `NetworkItem` and `Hint` serializing as
arrays and timestamps being RFC 1123, both of which are artifacts of how Flask
renders Python and both of which pahoa reproduces deliberately. They carry
`Access-Control-Allow-Origin: *`, so a tracker page served from another origin
can fetch a room directly. Rendered documents are cached for 60 seconds, and the
static half for 300, matching the windows the reference memoizes with.

**The tracker is gated behind the admin token whenever one is configured**, and
open when none is. An unauthenticated tracker on a public port lets a port scan
read the participant list out of every room, and that turns a port range into an
index from a player's name to a room's address. Rooms run without a password —
the usual case — are protected today only by being unidentifiable, so the gate
holds whether or not the seed is a race. A standalone pahoa configures no token
and serves it openly; `--open-tracker` restores that alongside an admin API. [docs/tracker.md](docs/tracker.md) covers the shapes,
the CORS rules, and the live-tracker direction this is a stepping stone to.

**The admin surface is authenticated by `PAHOA_ADMIN_TOKEN` and nothing else.**
It is mutating and reachable from the internet by design — driving it with
`curl` is a capability worth keeping — so three things follow. The token needs
at least 32 bytes and pahoa refuses to start with a shorter one; comparison is
constant-time; and failures are rate-limited, after which the surface stops
answering for the rest of the window even to the correct token, so the limit
cannot be used to test guesses.

**With no token configured the admin routes return `404`, not `401`.** The
surface is *absent* rather than locked, so a misconfiguration fails closed and
is indistinguishable from a build that never had one.

```sh
curl -s -H "Authorization: Bearer $PAHOA_ADMIN_TOKEN" https://host:38281/admin/v1/status | jq
```

#### Commands

`POST /admin/v1/command` takes a tagged object, not a command line — so an
unknown command is a `400` rather than a confusing text reply, and a caller can
validate before sending.

| command | body |
|---|---|
| `status` | `{"command":"status"}` |
| `say` | `{"command":"say","text":"…"}` |
| `countdown` | `{"command":"countdown","seconds":10}` |
| `release` | `{"command":"release","slot":3}` |
| `collect` | `{"command":"collect","slot":3}` |
| `send_item` | `{"command":"send_item","slot":3,"item":"Lamp"}` |
| `hint` | `{"command":"hint","slot":3,"item":"Progressive Sword","force":false}` |
| `kick` | `{"command":"kick","slot":3,"reason":"…"}` |

Every one answers the same shape, and `output` is pahoa's own phrasing so an
organizer reads what a player would:

```json
{"ok": true, "output": ["Released 130 locations for Troy."], "affected_slots": [3]}
```

**A command the room refuses is a `200` carrying `ok: false`**, not a `4xx` — it
was understood and answered. Only a malformed request is the caller's fault.

An administrator is not bound by the modes that gate players: `--release-mode
disabled` stops `!release` and does not stop this, because being able to act for
someone who cannot is the point. `hint` is the exception with two behaviors —
`force: true` grants the hint outright, while the default charges the slot's own
points exactly as `!hint` would. `kick` disconnects every connection a slot has
and is not a ban; nothing stops an immediate reconnect.

### Changing the rules on a live room

`!admin login <server-password>` from any connected client opens a remote
administration session, and `!admin /option <name> <value>` sets any of the room
options below except the passwords. The change is announced to the connected
clients that need it — a `RoomUpdate` carrying the permission map, or one per
slot carrying its recomputed hint points — and it **persists**, because the save
is authoritative for these fields and a restart restores them over whatever flag
the room was started with.

The passwords are refused, and the same fact is why: the save deliberately
carries no secret, so a live change to one would silently revert at the next
restart. Rotation belongs to whatever configures the room. The one exception is
`POST /admin/v1/slots/<n>/password`, which is per-slot and stays live —
[docs/slots.md](docs/slots.md) covers the difference.

The consequence is that **once a save exists, these flags no longer decide
anything.** Starting a room with `--hint-cost 10` against a save that says `15`
gets you `15`, which is what makes a live change worth making. Since that would
otherwise be a flag silently doing nothing, a startup `WARN` names any option
flag the save overruled and both values. To actually change one, use `!admin
/option` or start from an empty `--save-dir`.

### Room options

| option | default | |
|---|---|---|
| `--password <pw>` | — | Required from every client on connect |
| `--server-password <pw>` | — | Enables `!admin login`; unset refuses it outright |
| `--hint-cost <percent>` | `10` | Hint price as a percentage of a slot's own location count; `0` makes hints free |
| `--location-check-points <n>` | `1` | Points earned per check |
| `--release-mode <mode>` | `auto` | `auto`, `enabled`, `disabled`, `goal`, `auto-enabled` |
| `--collect-mode <mode>` | `auto` | as `--release-mode` |
| `--remaining-mode <mode>` | `goal` | `enabled`, `disabled`, `goal` |
| `--countdown-mode <mode>` | `enabled` | `enabled`, `disabled`, `auto` |
| `--no-item-cheat` | off | Refuse `!getitem` |
| `--compatibility <0\|1\|2>` | `2` | `0` exact client version match, `1` strict, `2` permissive |
| `--use-embedded-options` | off | Take every room option above from the seed instead |

The mode choices are narrower for `--remaining-mode` and `--countdown-mode`
because those two commands compare their mode for *equality*, where `!release`
and `!collect` test it with `"enabled" in mode`. A value outside the list would
match no branch and sit there doing nothing, so it is rejected instead.

`--use-embedded-options` reads the `server_options` a seed was generated with
and applies them **over** the flags above — the seed wins. That direction is the
reference's (`MultiServer.py:558-560`) and is the point of the flag: it honors
what the generator was configured with rather than what whoever restarts the
room happens to type. It matters more than it sounds, because real seeds rarely
agree with the defaults — of the four in `crates/pahoa-pickle/tests/fixtures`,
all four set
`collect_mode: disabled` against a default of `auto`, and their `hint_cost`
values are 5 and 20 against a default of 10. The room prints what it took, and
warns about anything it recognized but could not use.

### Inspecting

```sh
pahoa inspect seed.archipelago [--snapshot <datapackage.json>]
```

Slot, game, location and hint counts, plus what the data package resolved to.
`tools/inspect-multidata.py` is the reference implementation of this output and
`crates/pahoa/tests/inspect_differential.rs` compares the two line for line.

## Not implemented yet

Phase 1 — the protocol-complete headless server — is done: real clients play a
real seed to completion, and a 6000-connection load run sustains a mass release.
Deliberately absent so far, and reachable from no flag:

- **The PROXY protocol.** With TLS terminated here there is less call for it,
  but a room behind a load balancer still sees the balancer's address rather
  than the client's.
- **Most of the `/` console command set.** `!admin` dispatches into `/option`,
  `/options` and `/help`; the commands that act on a player — `/release`,
  `/collect`, `/send`, `/kick` — are on the admin REST API instead, which
  authenticates with a bearer token rather than a password typed into chat.
- The lobby integration.
- `--auto_shutdown` and `--disable_save`, which the reference has. Both are
  covered by omitting `--save-dir`. Its `--loglevel` is spelled `--log-level`
  here and aliased, and `--logtime` has no equivalent because every line is
  timestamped already.

## Development

`tools/README.md` covers the differential harness against the Python server, the
generators behind the committed test vectors, the load driver, and the Autobahn
WebSocket conformance run.
