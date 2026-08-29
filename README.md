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
| `--save-dir <dir>` | — | Where the room persists itself |
| `--save-interval <secs>` | `60` | Save cadence |
| `--ping-interval <secs>` | `20` | WebSocket keepalive cadence; `0` disables |
| `--ping-timeout <secs>` | `20` | Grace for the answering pong; `0` never drops |
| `--journal` | off | Append the room's history to `history.jsonl` beside the save |
| `--outbound-budget <MiB>` | derived | Cap on queued outbound data across all clients |
| `--shards <n>` | derived | Fan-out width, 1–32. Also the blast radius of a dropped broadcast |
| `--shard-queue-depth <n>` | derived | How far one shard may fall behind, 4096–65536 |
| `--log-level <level>` | `info` | `trace`, `debug`, `info`, `warn`, `error` |
| `--log-format <fmt>` | `text` | `text` for a terminal, `json` for a log aggregator |
| `--tls-cert <file.pem>` | — | Certificate chain; terminates TLS on the room port |
| `--tls-key <file.pem>` | — | Its private key. Both or neither |
| `--allow-plaintext` | off | Keep answering `ws://` after a certificate is set |

```sh
pahoa serve seed.archipelago --port 38281 --save-dir /var/lib/pahoa/room-1
```

### Fan-out

Broadcasts go to *K* shard tasks rather than to every connection, and each shard
expands the audience against the membership it owns. **`--shards` is a
reliability knob before a throughput one:** a broadcast a shard's inbox has no
room for is dropped, and the room answers by closing every connection that shard
owns, because the audience is expanded inside the shard and nothing upstream
knows who the frame was for. Blast radius is therefore connections ÷ shards.

Both default from the seed's slot count at three connections per slot: one shard
per 512 expected connections, and a depth sized for whichever of two bursts is
larger. A 2000-slot room gets 12 shards holding 32,000 each, ~500 connections
behind any one queue.

**Only one of those bursts divides by the width, and getting that wrong made
the derivation inert.** A reconnect storm is per-connection — every connection
returns at once, each buying a full replay — so a wider fan-out really does
lower what each shard needs. A release tail is per-broadcast and divides by
nothing: `Shards::broadcast` puts one copy into *every* shard's inbox, so the
broadcasts a room may have outstanding is exactly the depth however many shards
there are. Widening buys no broadcast headroom, and only multiplies its cost.

Sizing for the per-connection shape alone therefore moved the scarce number the
wrong way — and canceled out against `shards_for`'s own divisor, so every room
under ~5,461 slots landed on the 4,096 floor whatever its seed. The depth is now
the larger of the two shapes: `slots × 16` for the release tail, against
`connections-per-shard × 8` for the storm.

A release is what makes the second shape large. The full feed amortizes 140
items into one broadcast; the [scoped feed](docs/scoped-feed.md) cannot, since
each broadcast carries only what concerns one receiver slot, so a release costs
about one broadcast per receiver.

They used to follow the Tokio worker count, which follows the cgroup CPU quota,
and that was wrong: shard count is a topology decision and the quota is a
scheduling one. An orchestrator setting `limits.cpu: 2` for a 2000-slot room had
no way to widen the fan-out except by buying a CPU ceiling nothing would use —
and at two shards that room put half its connections behind one queue.

**Shard inboxes are not inside `--outbound-budget`.** The budget is charged when
a frame is queued *for a connection*, which is downstream of these queues; a
message still waiting in one has not been expanded to an audience yet. The
envelopes cost `shards × depth × 72` bytes, reserved up front, and the payloads
they point at are refcounted — one broadcast is a single allocation however many
shards hold it. **This is where broadcast headroom gets expensive:** it costs
`shards × depth` to buy `depth`, so a 2000-slot room reserves 26 MiB and a
6000-slot one hits the 144 MiB corner at the flag's limits. The startup log
reports width, depth, blast radius and envelope bytes on its `fanning out` line,
which is the number to add when sizing a container against the budget.

### Keepalives

**The server is the only side that pings.** Archipelago's clients connect with
`ping_interval=None` (`CommonClient.py:872`), turning theirs off deliberately, so
a room that does not ping leaves an idle connection completely silent in both
directions — and middleboxes reap silent flows, commonly at 60 seconds, telling
neither end. pahoa pings every 20 seconds and drops a connection that has not
answered within 20, matching the reference, which inherits both numbers from
`websockets`.

The timeout is **not** an allowance for a lost ping: TCP retransmits, so a ping
cannot vanish the way a datagram heartbeat can, and one outstanding probe is a
sufficient test. It is headroom for the peer's *application* to reply. It is also
the only signal there is — writing a ping to a dead peer **succeeds**, since the
bytes land in the local send buffer and TCP retries for minutes, so the absent
pong is the whole of the evidence. Worst-case detection is therefore
`interval + timeout`, because a peer that dies just after answering is not probed
again for a full interval.

`SO_KEEPALIVE` is deliberately **not** set. It would shed connections wedged by a
bug in pahoa itself, which sounds like robustness and is really a way to stop
anyone ever reporting the bug.

### The room journal

`--journal` appends the room's history to `history.jsonl` in the save directory,
one JSON line per event: checks, cheats, hints with the point balance either
side, chat, the DeathLink/TrapLink/RingLink conventions, goals, releases and
collects with what caused them, connects and disconnects, tag changes, every
mutating admin command, option changes, and the `started`/`stopped` pair that
divides the file into the runs that produced it. It needs `--save-dir`, and it is
**not** in the log stream.

A `release` and a `collect` carry a `trigger` — `goal`, `admin`, `player` or
`group` — because all three doors into one produce the same flood of checks and
the same announcement to clients, so nothing downstream can tell a player giving
up on their own world from an organizer clearing one. Both are written *before*
the checks they cause, so the line explaining a flood sits above it, and a
refused release writes nothing at all.

The link records carry **both** the sender the room authenticated and the
`source` the packet claimed, because the second is client-supplied and nothing
stops a client naming somebody else. Other `Bounce` traffic stays out: a link
fires on a game event, while a fork's own relay protocol is unbounded.

Because the file outlives any one process, `started` carries the version and git
revision that wrote what follows, and `stopped` carries those plus why the room
went down — `SIGTERM`, `SIGINT`, or `admin request`. **A `started` with no
`stopped` before it is an unclean stop**, which is the design rather than a gap
in it: nothing can write a closing record for a process that has already been
killed, so the absence is the signal.

That split is about access rather than durability. A log aggregator is the right
place for an operator debugging across rooms, but scoping one organizer to one
room's lines is not something Loki enforces, retention is a platform setting an
async room can outlive, and a restarted room is a new pod. A file in the room's
own directory has none of those problems.

**No password ever reaches it.** `chat` records the text the room *broadcast*, so
an `!admin` line is already masked, and the option records carry password modes
and booleans rather than values.

The actor pays 0.9% of a mass release to write it — it queues plain numbers and a
thread does the JSON — and the channel drops rather than blocks if a disk stalls,
recording the gap in the journal itself. [docs/journal.md](docs/journal.md) has
the measurements and the full record catalogue.

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

Once a certificate is configured, plaintext is **refused** unless
`--allow-plaintext` is given. That default is deliberate: the admin API is
mutating and internet-reachable, and serving its bearer token in the clear on
the same port would undo the point of having it.

**How it is refused depends on what asked, and the difference is load-bearing.**
An ordinary request — `curl`, a browser, a probe — gets RFC 2817's `426 Upgrade
Required` naming `Upgrade: TLS/1.3, HTTP/1.1`, which is the legible answer. A
**WebSocket upgrade is closed on without a reply**, which is not.

Archipelago clients are handed a bare `host:port` and try `ws://` first, because
`CommonClient.py:857` prepends it when the address carries no scheme. They
recover through one narrow heuristic: `websockets` raises `InvalidMessage` when
the reply is not parseable HTTP, and `CommonClient.py:887-890` reads that as
"probably encrypted" and retries the same address as `wss://`. Against a room
behind an ordinary TLS terminator the plaintext attempt gets alert bytes, the
retry fires, and the player never learns it happened. A well-formed `426`
defeats exactly that — the library parses it happily and raises
`InvalidStatusCode`, which is *not* the branch that retries. So the
standards-correct status is the one that strands clients the reference's
accidental behavior would have connected, and the upgrade path deliberately
gives them the unparseable answer they are watching for.

With no certificate configured nothing changes — plaintext is served, and a TLS
client gets an immediate `handshake_failure` alert so a client probing `wss://`
before `ws://` falls back at once rather than hanging. The two directions are
the same courtesy.

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

Item and location names, ids and name groups come from the seed's own embedded
data package, so a custom apworld this build has never heard of still resolves
its own names. The one thing Archipelago serializes nowhere is each world's
`hint_blacklist`, so **that table is compiled into pahoa** and regenerated from a
checkout by `tools/export-datapackage.py`. A game absent from it hints
everything, which is what the reference gives a world that sets none.

The trade is that the blacklist tracks the pahoa build rather than the apworld
that generated the seed, so a world newly adding one needs a pahoa release to be
honored. Two worlds set one today and both have for years. A seed whose package
WebHost stripped on upload is reported at startup and its names degrade to
`Unknown item (ID:n)`; the room still hosts.

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

**The save format is versioned, and a newer save is refused rather than
half-read.** Version 2 added the per-slot lock state. A current server reads
older saves fine, but rolling a room *back* to a binary that predates a field
fails at startup with "save format version N is newer than this server
understands" — which is the intended trade for a field carrying access control,
since a lock that quietly stopped holding after a downgrade is worse than a room
that will not start. Roll forward, or start the room on an empty directory.

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
| `GET /admin/v1/status` | bearer | Clients, save state, net counters, activity, per-slot progress, the room's effective options |
| `GET /admin/v1/metrics` | bearer | The same numbers as Prometheus text, plus the per-slot series |
| `POST /admin/v1/command` | bearer | The typed command set below |
| `GET PUT PATCH DELETE /admin/v1/filter` | bearer | The room-wide send and receive filter |
| `GET PUT PATCH DELETE /admin/v1/slots/<n>/filter` | bearer | One slot's own |
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
constant-time; and authentication *failures* are rate-limited, ten per source
address per minute, past which that source is answered `429` with a
`Retry-After` rather than `401`.

**A request presenting the correct token is never refused.** The limit is
checked after the token and keyed on the connection's peer address, both
deliberately. Checking it first — one budget for the whole room, before looking
at what was sent — meant anyone who could reach the port could take the admin
surface down for everybody, orchestrator included, with eleven wrong guesses a
minute and no credential. Keying on the source gives an attacker only their own
budget to spend, and a room-wide ceiling of 500 failures a minute sits behind
that as an anti-flood backstop, again on the failure path only. None of this is
what protects the token: at 32 bytes minimum, guessing rate is not the threat.
The peer address is the TCP one, never `X-Forwarded-For` — this port is reachable
directly, so a forwarding header on it is attacker-controlled text.

**With no token configured the admin routes return `404`, not `401`.** The
surface is *absent* rather than locked, so a misconfiguration fails closed and
is indistinguishable from a build that never had one.

```sh
curl -s -H "Authorization: Bearer $PAHOA_ADMIN_TOKEN" https://host:38281/admin/v1/status | jq
```

**`status.activity` answers two different questions, and an idle reaper wants
the second one.** `last_client_message_at` / `idle_seconds` move on *any* packet
from any client — chat, `Sync`, `Get`, `StatusUpdate` — so they say whether the
sockets are alive, which is what they are named for.
`last_check_at` / `check_idle_seconds` move only when a slot registers a
genuinely **new** location check, which is what the reference auto-shuts rooms
down on (`MultiServer.py:2671-2682`) and the only one of the two that a room
full of people idling in chat will let go stale. The timer is per-slot inside
the room, room-wide on this surface, and persisted — so it survives a restart,
which no orchestrator polling check *counts* could reconstruct.

Both are `null` when nothing has happened yet, and for the check pair that is a
real answer rather than a gap: a room whose organizer is still getting people
connected has never had a check, and that is not the same as a check at the
epoch. Callers that reap on this should decide what an unplayed room means to
them rather than reading a zero.

`check_idle_seconds` is measured against the wall clock, not against uptime, so
a room that was **stopped** for three days reports three days of check-idle the
moment it comes back. That is the honest answer — the room genuinely was not
played — but it means a freshly started room can be reap-eligible on arrival,
which is the opposite of what "it just started" suggests. Anything reaping on
this wants a floor on how long the room has been up before the number counts.

#### The per-slot series

`/admin/v1/metrics` carries labeled counters alongside its fixed ones. They are
the only metrics whose *number of series* depends on the room.

```
pahoa_packets_in_total{team="0",slot="4",player="MooingYacht",game="Yacht Dice",cmd="LocationChecks"} 13512
pahoa_packets_preauth_total{cmd="Connect"} 91
pahoa_filtered_total{team="0",slot="4",player="MooingYacht",game="Yacht Dice",direction="from_slot",kind="bounce"} 22
pahoa_frames_out_total{team="0",slot="4",player="MooingYacht",game="Yacht Dice"} 15
pahoa_bytes_out_total{team="0",slot="4",player="MooingYacht",game="Yacht Dice"} 3744
pahoa_redundant_requests_total{team="0",slot="4",player="MooingYacht",game="Yacht Dice",kind="location_check"} 9
```

**`pahoa_redundant_requests_total` counts work the room had already done**, and
it is the one series here that is not about load. `kind="location_check"` is a
location that slot had already checked; `kind="hint"` is a `CreateHints`, or a
`create_as_hint=2` `LocationScouts`, naming a hint that already existed.

Neither is an error — the room filters both and stays correct — which is exactly
why they are worth counting. **A client can be badly wrong in a way that
produces no wrong behavior**, so a world's client re-sending its whole check list
every tick, or re-scouting in a loop, costs the room work and appears nowhere:
not in the log, not in the journal, and not in any error count, because there is
no error. It looks like a busy player.

**Read it as a ratio against `pahoa_packets_in_total` for the same slot, never as
a threshold.** Re-sending checks on reconnect is how the protocol
resynchronizes, so a room with churn accumulates these legitimately; one
redundant batch per connection is correct and a thousand is a loop. The `game`
label is the axis that finds the bug: a whole game's slots sharing a ratio is a
client-implementation problem worth reporting upstream, while one slot out of
forty is a mod or a script. Location ids this seed does not contain are **not**
counted — clients legitimately send those, and including them would put a
permanent floor under the signal.

**Labeled at the slot rather than pre-aggregated**, because the finest honest
granularity aggregates upward for free and nothing recovers detail that was
summed away. `sum by (cmd)` is "incoming packets by command"; `sum by (game)` is
"by game"; and neither of those can answer the question a struggling room
actually raises, which is *which slot* is producing the Bounce storm.

**`player` and `game` are functions of `(team, slot)`** — one each — so the four
together are one dimension of size "slots in this room" rather than the product
four labels look like. The real product is slots × commands, and it stays small
because **only observed pairs are emitted**: a slot that has never sent a
`SetNotify` has no series rather than a zero, which on a 2000-slot room is the
difference between ~28,000 series and a fraction of that. A gap and a zero also
mean different things on a dashboard.

**`team` is always `0`**, and is a label anyway. See [Teams](#teams): a room has
one, and a scraper that groups by it needs nothing rewritten if that ever
changes, where one that assumed slot numbers were unique would silently add two
teams together.

**`pahoa_client_connections_total{...,deflate="true"|"false"}`** counts each
connection that reached a slot by whether it negotiated permessage-deflate, so
`sum by (game, deflate)` is which games' clients support compression.

It is **per connection, not per slot** — a game client may compress where a
tracker on the same slot does not — and a counter rather than a gauge, so it
survives churn and answers the question over a room's life. The two facts it
joins are settled in different places: the extension during the WebSocket
handshake, before `Connect`, when no game is known; the game with `Connect`,
known only to the room. They meet on the shard's per-connection record, which is
where this is counted, on the transition that means "this connection
authenticated".

What it is really reporting is **which clients receive compressed payloads**,
which is where the cost is: an outbound broadcast is compressed once per shard
and handed to every recipient that negotiated it, while everyone else gets the
plain frame. A connection that declined is one the room cannot share that work
with.

**`pahoa_bytes_in_total` counts wire bytes**, as framed and compressed on the
socket, so it is comparable with `pahoa_bytes_out_total` rather than with the
JSON the room parses. A client on permessage-deflate can send a 4 KiB `Say` for
fifty bytes, and this reports the fifty. Pings, pongs and undecodable frames are
excluded — the reader task that sees those bytes has a connection id and no slot
— which makes this exactly the byte counterpart of `pahoa_packets_in_total`,
with the same pre-auth split.

**Outbound is two counters, not one, because there is no single number.** A
slot's connections are not sent the same stream — a `NoText` tracker is left out
of chat, and a scoped connection takes items by a different route than a
full-feed one — so "packets sent to slot 4" has no honest value.

- **`pahoa_packets_out_total{cmd}`** is what the room *produced*, once per
  message whatever its audience. One chat line to two thousand slots is one. No
  slot label, and none is possible: attributing per recipient would mean
  expanding every broadcast's audience on the actor, which is the
  O(connections) walk the shards exist to avoid.
- **`pahoa_frames_out_total` / `pahoa_bytes_out_total`** are what fan-out made
  of it, per *recipient connection*. Bytes are post-compression and are what
  fills the outbound budget, so these are the pair to read next to
  `pahoa_outbound_queued_bytes` and `pahoa_lag_disconnects_total`.

Together they say whether a load problem is production or fan-out, which the
room-wide gauges cannot. `pahoa_filtered_total{direction="to_slot"}` shares the
per-connection denominator, so the share of a slot's traffic being filtered is a
ratio of the two.

**`pahoa_shard_overflow_total` should be zero, and is not a lag disconnect.** A
lagged client is one that could not keep up; this is the *server* failing to —
a fan-out shard's inbox with no room in it. The frame is lost, so the room
disconnects whoever it was for: one connection for a directed send, every
connection on that shard for a broadcast, since the audience is expanded inside
the shard and nothing upstream knows who it was for.

That is deliberately expensive, because the alternative is worse. The room
advances a slot's send index as it sends, so a dropped `ReceivedItems` leaves
the room believing a slot holds items it never received and the client cannot
tell — it would play a different game until it happened to reconnect. Closing is
safe where dropping is not, because `Connect` resends `checked_locations` in
full and replays the item queue from zero. If this counter moves, the shard
queue is too shallow for the load.

**Read it against `pahoa_shard_sweeps_total`, which is the one that counts
people.** A shard whose inbox is full refuses a broadcast on every attempt, and
each refusal used to walk the whole membership again — on the unbounded control
queue, which is selected ahead of frames, so the response to congestion competed
with its own recovery. A sweep now happens once per population and stands down
only when somebody new arrives, so overflows count how far past the first
failure the load went while sweeps count how many times connections were
actually closed. Multiply sweeps by `--shards`' blast radius for the number of
players affected.

Pre-auth deliveries have their own pair, `pahoa_frames_out_preauth_total` and
`pahoa_bytes_out_preauth_total`: every connection is sent `RoomInfo` before it
holds a slot, and a `DataPackage` answered there can run to megabytes.
**`Connected` is not among them** — the transport is told a connection's
membership before that reply is dispatched, precisely so the largest packet a
slot receives lands on the slot.

**Packets arriving before a connection holds a slot get their own metric.**
`Connect` and `GetDataPackage` are the only two the room answers
unauthenticated, so a climbing `pahoa_packets_preauth_total{cmd="Connect"}` is
failed logins — worth seeing, and worth *not* filing under an empty slot label
that every per-slot query would have to remember to exclude. A `Connect` is
never also counted against the slot it just created.

`pahoa_filtered_total` is the same breakdown for drops, and
`pahoa_filtered_from_slots_total` / `pahoa_filtered_to_slots_total` are it added
up rather than counters of their own — so a drop path that forgot to attribute
itself goes missing from both instead of showing up as a discrepancy nobody is
watching for.

Two things a scraper should know. **`player` and `game` come out of an uploaded
seed**, so they are untrusted text: quotes and backslashes are escaped, and
values are cut at 128 characters, well past any real name. And these counters
are **per process** — cumulative, monotonic, and back to zero on a restart, with
the whole endpoint absent on a room that predates them.

#### Process cost

```
process_cpu_seconds_total 0.01
process_start_time_seconds 1787641309.060
process_resident_memory_bytes 24174592
```

**These keep Prometheus's conventional names rather than the `pahoa_` prefix**,
because they are what every client library exports and every off-the-shelf
dashboard already plots: `rate(process_cpu_seconds_total[5m])` is cores used,
and pairing it with the start time gives a whole-life CPU share from a single
sample.

`process_resident_memory_bytes` is the only spelling of resident memory.
Earlier builds also exported `pahoa_resident_bytes` carrying the identical
number; that has been removed rather than kept as an alias, because two series
that can never disagree give a reader no way to tell them from two that might.
A scrape naming the old one gets nothing and should be repointed.

**Process-wide, and that is the limit of what it says.** It answers what a room
costs a node, not which task is busy — and the task that matters is the single
actor owning room state. A room can be CPU-bound in its shards, which is
compression doing its job, or backed up on its actor, which is the bottleneck
the whole design exists to avoid; only `pahoa_mailbox_depth` and
`pahoa_mailbox_peak` tell those apart. Read CPU for capacity and the mailbox
for health.

Both come from `/proc/self/stat`, and are **absent rather than zero** where it
cannot be read — a zero would report a busy room as an idle one. An idle room
genuinely does read `0.00`: the counter is quantized to the 10 ms clock tick,
and a room with nobody connected does not reach one.

#### The HTTP surface's own metrics

The admin API and the game share a port, and are counted apart. They are
different workloads — an orchestrator on a reconcile loop and whatever the
internet points at a public listener, against players — and summed together each
would hide the other.

```
pahoa_http_requests_total{route="/admin/v1/slots/{slot}/filter",method="PUT",status="200"} 1
pahoa_http_requests_total{route="other",method="GET",status="404"} 1
pahoa_http_request_bytes_total{route="/healthz"} 86
pahoa_http_response_bytes_total{route="/api/tracker"} 4823901
pahoa_admin_auth_failures_total 1
pahoa_admin_auth_rate_limited_total 0
pahoa_http_malformed_total 0
```

**`route` is a template, never the path as sent.** A public port gets scanned,
and a label taken from the request line would let anyone mint series until a
scrape fell over — so `/admin/v1/slots/7/filter` counts under
`/admin/v1/slots/{slot}/filter`, and anything unrecognized under `other`. Bytes
are summed by route only: a tracker document is megabytes where a health check
is bytes, and the method and status say nothing about what a request weighed.

**A WebSocket upgrade is not counted here.** It is an HTTP request in form only,
and everything it goes on to carry is already the game's.

`pahoa_admin_auth_failures_total` gets its own counter rather than being read off
`status="401"`, because that status also carries the tracker's gate — this is the
one to alert on. `pahoa_admin_auth_rate_limited_total` counts the `429`s, which
given that a correct token is never refused means sources that were guessing.
`pahoa_http_malformed_total` is requests that never parsed into a route at all,
so they appear nowhere else.

#### Teams

**A multiworld has exactly one team, and pahoa refuses a seed that says
otherwise.**

Archipelago's data model is team-aware from top to bottom: `(team, slot)` keys
everything the server owns, `Connected` and `NetworkPlayer` carry a `team`
field, and `MultiServer.py` threads a team through hints, item queues, status
and chat. But nothing can produce a second one. Generation writes
`{name: (0, player)}` unconditionally (`Main.py:337`), and the server seeds
`self.clients = {0: {}}` at load and never grows it (`MultiServer.py:521`) — so
a seed naming any other team raises inside `ctx.clients[team][slot]` on the
connect that used the name, with the room already up and the traceback in a log.

pahoa serves what the reference serves, so it serves one team, and says so at
load instead:

```
connect_names["Troy"] is on team 1, and this server serves one team, as the reference does
```

Two consequences worth knowing.

**Every surface carries the team even though it is always `0`** — the metric
label above, the `team` on each `/admin/v1/status` and `/api/v1/room` slot row,
the tracker's rows, the filter and password replies, and an optional `"team"` on
every admin command that takes a slot. A caller that names team 1 is told it
does not exist rather than having the field dropped and its command run against
team 0. None of this is speculative generality: it is what stops a caller
inferring that slot numbers are unique, which is the assumption that would need
finding and fixing everywhere on the day upstream grows a second team.

**Internally the walks are already right.** `teams()` yields the one team and
callers iterate it rather than writing `0`, hint rechecks are keyed on the
finding team, and precollected hints and always-goal slots are seeded per team.
So the limit lives in two places — that accessor and the load-time check — and
not in a literal scattered through every handler.

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
| `send_multiple` | `{"command":"send_multiple","slot":3,"item":"Rupee","amount":5}` |
| `hint` | `{"command":"hint","slot":3,"item":"Progressive Sword","force":false}` |
| `hint_location` | `{"command":"hint_location","slot":3,"location":"Attic","force":false}` |
| `send_location` | `{"command":"send_location","slot":3,"location":"Attic"}` |
| `allow_release` | `{"command":"allow_release","slot":3,"allowed":true}` |
| `lock` | `{"command":"lock","slot":3,"locked":true}` |
| `set_status` | `{"command":"set_status","slot":3,"status":"goal"}` |
| `alias` | `{"command":"alias","slot":3,"alias":"Organizer"}` |
| `option` | `{"command":"option","name":"hint_cost","value":20}` |
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

**`hint` and `hint_location` are separate verbs, and each accepts only its own
kind of name** — or a numeric id, which addresses its target directly. The chat
commands guess at what a player typed; this one does not, because its caller is
a program and a near-miss should be a visible error rather than a silent
decision to act on something else. `send_location` is the same distinction one
step further: it *checks* the location, sending out whatever items it holds,
rather than merely hinting at it.

**`allow_release` is an exemption, not a third permission.** `release_mode` is
the room's rule for everyone; this exempts one slot from it, and the exemption is
checked first — so an allowed slot may `!release` under a mode that forbids
everyone else. `{"allowed": false}` clears the exemption and returns that slot to
the mode, which may still permit releasing; it does not forbid it. The reference
spells these as two commands, `/allow_release` and `/forbid_release`, and the
second name is why this is one command with a boolean instead. There is no
collect equivalent, in pahoa or in the reference — `!collect` consults
`collect_mode` and nothing else.

**`set_status` declares a slot's completion state on its behalf**, which the
protocol otherwise lets only that slot's own client do — `StatusUpdate` is
upstream's single external writer of it, so a player who has finished but whose
client cannot say so leaves an organizer with nothing to do about it. Statuses
are named rather than numbered: `unknown`, `connected`, `ready`, `playing`,
`goal`. `unknown` and `connected` are derived from the connection and will be
rewritten the next time the slot connects or disconnects, and the response says
so.

Declaring a goal goes through the same path a client's own `StatusUpdate` takes,
so the room announces it and the `collect_mode` / `release_mode` auto rules fire
exactly as they would otherwise — the response names them, because a world
quietly emptying out is the surprising part.

**Goal cannot be undone, including from here.** Upstream guards every status
change with `if current != CLIENT_GOAL` — not even the client that declared it
may take it back — and pahoa keeps that invariant rather than carving out an
operator exception, so anything downstream can treat goal as monotonic. Where
the reference silently ignores the attempt, this refuses it and says why.

**`lock` bars a slot from connecting and does not disconnect anyone.** Those are
separate decisions and separate commands: locking refuses the *next* login,
`kick` ends the current session, and an administrator dealing with a griefer
wants both in that order — kicking first leaves a window in which they simply
reconnect. The response says so when a locked slot still has connections open,
because the obvious reading of "locked" is that the room ejected them.

A lock is **orthogonal to every password mode** rather than a fourth one. It
applies to a room with no password, and it applies to somebody holding the
correct one, which is what makes it the answer to "this person, specifically, is
not to come back". It **persists in the save**, because the reason to set one
outlives any single process and a room that quietly re-admits a locked player on
its next deploy fails in exactly the moment it was set up for. It is reported per
slot as `locked` in `/admin/v1/status`, and counted by `pahoa_slots_locked` —
worth watching, since the failure mode of a temporary lock is nobody remembering
to lift it.

`{"command":"lock","slot":3}` locks; unlocking is the explicit
`{"locked": false}`, which is the right way round for a command whose job is
keeping somebody out.

The protocol has no refusal reason for this — the list is closed — so a locked
slot is refused with **`InvalidSlot` and `SlotLocked` together**. `InvalidSlot`
is what makes stock clients stop cleanly instead of reconnecting on a doubling
delay, and `SlotLocked` is what lets anything reading the raw list tell a lock
from a typo. The cost, accepted deliberately, is that a stock client tells a
locked player their slot name is invalid; the room's own log records the truth.

`alias` sets or clears *another* player's alias, which `!alias` only lets a
player do for themselves; an empty or omitted alias clears it, and the name is
truncated to 16 characters exactly as the chat command truncates it.

**`send_multiple` is `send_item` with a count, capped at 100**, and one copy
reads identically either way — the reference's `/send` is literally
`/send_multiple 1`, so the two share an implementation and cannot word the same
grant differently. The cap is worth keeping rather than raising: every copy is
queued on both of the slot's item streams and replayed from index zero on each
reconnect, so a stray extra digit is a room that never finishes sending. `amount`
is required, because a default of one would make a `send_multiple` that did a
fraction of its job look like it worked.

Both announce the grant to the room as **plain text**, with no message type, no
`item` and no receiving slot. That is not an omission: the reference announces
the same event two ways depending on who asked — `!getitem` sends a typed
`ItemCheat` carrying the `NetworkItem`, while `/send` and `/send_multiple` send
a bare `PrintJSON` with only the text. A client keying off `type == "ItemCheat"`
therefore treats the two differently, so pahoa matches upstream on both rather
than quietly upgrading the console path to the richer form.

### Send and receive filters

A filter drops some of a slot's traffic. Two problems share the mechanism: a
client that crashes on a particular message needs that message not to reach it,
and a room drowning in DeathLinks needs fewer of them to go out. Both are "drop
some of this for this slot", so every rule carries a probability and a plain
filter is simply one that always fires.

```jsonc
{"direction": "from_slot", "kind": "bounce", "tag": "DeathLink", "p": 0.25}
// of the DeathLinks this slot SENDS, drop one in four and deliver three

{"direction": "to_slot", "kind": "print_json", "subtype": "Chat"}
// drop chat before it REACHES this slot; `p` omitted means p = 1, drop all
```

**`p` is the share dropped, not the share kept.** `p: 0.25` delivers 75% of what
matches; `p: 1` is a plain filter and is what an omitted `p` means; `p: 0` never
fires, which is how a more specific rule exempts itself from a blanket one.

**`from_slot` and `to_slot`, never `in` and `out`.** Those are relative and
nobody remembers to what — a server author reads "inbound" as arriving at the
room, an organizer reads it as what a player is sending, and the two are
opposites. A rule read backwards is a filter that silently does nothing.

**Only advisory traffic can be filtered.** `kind` is one of `bounce`, `say`,
`print_json`, `set`, `set_reply`, `retrieved`, `status_update`. Item deliveries,
`Connected`, scout results and room updates are **recognized and refused** rather
than quietly ignored, because dropping one desynchronizes the client — the room
advances a slot's send index as it sends, so the client would never learn what it
missed. If a client cannot survive one of those, no filter can save it, and the
honest answer is that the client is broken.

**`say` mutes a slot; a `to_slot` `print_json`/`Chat` rule deafens one.** Chat
crosses the room as two different kinds — `say` on the way in, `print_json` on
the way out — so which one to name depends on whether the slot is the source or
the audience. Naming the wrong one produces a filter that works perfectly on the
wrong person.

**Muting also disables that slot's `!` commands**, which is worth knowing before
using it. Every `Say` is chat first and a command second — the room broadcasts
the raw line before looking at whether it starts with `!` — so there is no point
downstream where the two are still separable. A muted slot cannot `!hint` or
`!release`.

Rules are a **set keyed on the matcher**, not an ordered list, and **the most
specific wins**: a rule naming a `tag` or `subtype` beats one naming only a
kind. So a blanket thin and an exemption for one tag coexist in either written
order, and `PATCH` never has to guess where a rule belongs.

| method | effect |
|---|---|
| `GET` | Read. Never a `404` for an unset filter |
| `PUT` | Replace the ruleset wholesale |
| `PATCH` | Merge, keyed on each rule's matcher. **Idempotent** |
| `DELETE` | With a body, remove the named matchers; with none, remove the ruleset |

A slot's filter **replaces** the room's rather than adding to it, which is what
makes "thin everyone except this slot" expressible without a negation. The reply
keeps the three states apart:

```jsonc
{"rules": null, "effective": [...], "inherited": true}   // no ruleset of its own
{"rules": [],   "effective": [],    "inherited": false}  // exempt from the room's
{"rules": [...], "effective": [...], "inherited": false} // its own
```

`rules` is what `PUT`/`PATCH`/`DELETE` edit; `effective` is what actually
applies. They are separate because a `GET` that returned the inherited rules
would make `PATCH` either merge into them — silently forking the room's filter
onto the slot, so later room changes stopped reaching it — or ignore what it had
just shown.

Filters **persist in the save**, and an explicitly empty one persists as an
exemption rather than collapsing back into inheritance. `/admin/v1/status`
reports `filtered` per slot and a `filters` block with how much has been dropped;
`pahoa_filtered_from_slots_total` and `pahoa_filtered_to_slots_total` are the
same numbers for a scraper.

**The two directions count different things, and have to.** `from_slot` is once
per *message* — a slot sends one `Say` and the room drops it once, before anyone
was going to receive it. `to_slot` is once per *recipient connection*, because
that test runs inside the shard's per-recipient loop: one chat line filtered for
forty slots is forty, and eighty if each of them also has a tracker attached.
That is the same denominator `pahoa_frames_out_total` uses, which is what makes
"what share of this slot's traffic is being filtered" a ratio worth taking.

Those two totals are **sums of `pahoa_filtered_total`**, not counters of their
own, so "how much is being dropped" and "which kind is being dropped" cannot
disagree — see the next section.

### Changing the rules on a live room

There are two ways in, for two different people holding two different
credentials, and they run the **same code**:

- **`!admin login <server-password>`** from any connected client opens a remote
  administration session, and `!admin /option <name> <value>` sets an option.
  This is the organizer's path — someone running the game from inside it, with a
  chat window and no bearer token.
- **`{"command":"option","name":"…","value":…}`** on the admin API is the
  operator's and the orchestrator's path, for reconfiguring a running room
  without a restart.

Both reach one function, so the settable names, the value parsing, the refusals,
the `RoomUpdate` fan-out and the journal records are identical by construction
rather than by agreement. The change is announced to the connected clients that
need it — a `RoomUpdate` carrying the permission map, or one per slot carrying
its recomputed hint points — and it **persists**, because the save is
authoritative for these fields and a restart restores them over whatever flag
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
pahoa inspect seed.archipelago
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
  than the client's. That matters in one place: the admin limiter counts
  authentication failures per source address, and behind a balancer that does
  not preserve the client address every caller would share one budget — which
  is the behavior the per-source keying replaced, wearing a better name. A room
  logs the address it sees when a source trips the limit, which is how to check.
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
