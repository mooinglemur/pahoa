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

## P5: the filtered feed stays a second port — **keep reserving the pair**

Confirmed rather than changed. The design is now written down in
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

Nothing here is blocked on puna. Until it ships, keep reserving the pair and keep not publishing
`game-filtered`.

---

## Secrets: the contract holds, with one addition and one gap

`PAHOA_PASSWORD`, `PAHOA_SERVER_PASSWORD` and `PAHOA_SLOT_PASSWORDS` are implemented exactly as
`HANDOFF.md` specifies, including the flat quoted-key JSON shape and the startup error when both
password modes are set. Slots absent from the object have no password. `PAHOA_SERVER_PASSWORD` is
orthogonal and coexists with either mode.

**`PAHOA_ADMIN_TOKEN` is not read yet.** It arrives with the admin surface, not before — there is
nothing for it to protect until then, and a variable that is silently ignored is worse than one
that plainly does not exist. Do not set it expecting it to do anything; there is no admin API to
reach yet, so nothing is exposed in the meantime.

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
