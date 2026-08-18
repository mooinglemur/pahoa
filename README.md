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
| `--snapshot <file.json>` | — | Data package snapshot, from `tools/export-datapackage.py` |
| `--save-dir <dir>` | — | Where the room persists itself |
| `--save-interval <secs>` | `60` | Save cadence |
| `--outbound-budget <MiB>` | derived | Cap on queued outbound data across all clients |
| `--log-level <level>` | `info` | `trace`, `debug`, `info`, `warn`, `error` |
| `--tls-cert <file.pem>` | — | Certificate chain; terminates TLS on the room port |
| `--tls-key <file.pem>` | — | Its private key. Both or neither |
| `--allow-plaintext` | off | Keep answering `ws://` after a certificate is set |

```sh
pahoa serve seed.archipelago --port 38281 --save-dir /var/lib/pahoa/room-1
```

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

Slots absent from the object have no password. Setting a room-wide *and* a
per-slot password is an error at startup rather than a silent preference for
one. `--server-password` is a third, orthogonal thing — it gates `!admin`, not
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
| `GET /admin/v1/status` | bearer | Clients, save state, net counters, per-slot progress |
| `GET /admin/v1/metrics` | bearer | The same numbers as Prometheus text |
| `POST /admin/v1/shutdown` | bearer | Quiesce, save, exit 0 |

`/healthz` needs no state to answer: the listener binds only after the save has
been restored, so reaching it at all is the readiness signal.

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
- **The scoped feed** — a second port on which a client receives only what is
  relevant to its own slot, for clients that cannot handle the `PrintJSON`
  firehose. Designed in [docs/scoped-feed.md](docs/scoped-feed.md).
- **The `/` console command set**, and therefore what `!admin` dispatches into.
  It overlaps the admin REST API heavily and is worth writing once, with it.
- The lobby integration, the tracker APIs and the admin REST API.
- `--auto_shutdown` and `--disable_save`, which the reference has. Both are
  covered by omitting `--save-dir`. Its `--loglevel` is spelled `--log-level`
  here and aliased, and `--logtime` has no equivalent because every line is
  timestamped already.

## Development

`tools/README.md` covers the differential harness against the Python server, the
generators behind the committed test vectors, the load driver, and the Autobahn
WebSocket conformance run.
