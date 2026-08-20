# Handoff: `puna` → `pahoa`

Changes pahoa needs so that **puna** — the Kubernetes room orchestrator and web app at
`/home/troy/src/puna` — can provision, observe and manage pahoa rooms. Earlier revisions cited this
tree by file and line; the behavioral detail now lives in pahoa's own `docs/` and in
`NOTES-FOR-PUNA.md`, and this document points at them rather than restating them.

Last updated 2026-08-19 against `45a5cff`, during puna's M10 — **the first deployment to a real
cluster**, which is where both open items came from. Neither was findable by review: one needed a
room to actually start, the other needed one to actually fail.

**P1–P18 are all resolved**, P18 by being declined, which was the right answer and the first of the
three options this document ranked. `NOTES-FOR-PUNA.md` is the reply in the other direction and is
the authority on what shipped and how; nothing in it is disputed here.

**Two items are open, both small.** P19 is a heads-up request following a decision already taken;
P20 is a defect in the JSON log stream. Neither blocks puna.

---

## Round two, closed

Puna's ingest met a real generation zip containing a **spectator slot** and following that slot
through pahoa raised three questions. All three came back answered, one of them differently and
better than proposed.

| # | Asked | Answer |
|---|---|---|
| **P16** | Reconcile what "a slot" means across the reporting surfaces | `connectable_slots` for roster questions, `player_slots` for progress, rule written down in [`docs/slots.md`](docs/slots.md) |
| **P17** | Decide whether `slot_passwords` should fail closed | **It does now** — and the contract inverted with it. See below |
| — | The race-mode tracker exposure, open since round one | Gated on **deployment, not seed**: any configured admin token gates the tracker |

**P17's answer was not the one this document leaned toward, and the reasoning was better.** The ask
was a startup cross-check — refuse to boot when `PAHOA_SLOT_PASSWORDS` does not cover every name in
`connect_names`. Pahoa declined it: coupling the secret to the seed at startup makes the two
undeployable independently, and the fail-closed connect check removes the open door anyway, which
was the actual concern. Agreed, and puna's plan now records that reasoning rather than the request.

**The tracker answer was also broader than asked.** The request was to gate `race_mode` seeds. Pahoa
gated on whether a token is configured at all, and the reason is worth stating precisely, because an
earlier revision of this document got it wrong: it is **not** primarily that slot names are personal
data. It is that **an open tracker turns port scanning into room identification.** A scan alone finds
open ports and cannot tell which multiworld is behind any of them; a participant list can, so a
high-visibility game — a streamer's, say — becomes findable. And reference Archipelago rooms are
commonly run with no password at all, as archipelago.gg's own default encourages, so *findable* is
*joinable*. The obscurity of the port is doing real work in protecting an unpassworded room from
griefing, and an open tracker removes it.

That threat has nothing to do with `race_mode`, which is why gating on the seed would have closed the
narrow hole and left the wide one open. Right call; puna wants nothing from `--open-tracker`.

### What puna changed in response

Recorded here so pahoa can see the contract is actually being held on this side:

- **`PAHOA_SLOT_PASSWORDS` is rendered complete or not at all** — never `{}`, never partial. Under
  fail-closed either one is a room nobody can join, and an empty object is exactly the shape a
  half-assembled slot list produces. It is a unit test on puna's Secret builder rather than a
  convention.
- **Spectators are in the map**, because puna's ingest keeps them (matching `WebHostLib/upload.py`,
  which skips only `SlotType.group`). Note that the *direction* of that failure has now flipped in
  puna's favour: a slot puna forgets is a player who cannot connect, which is loud, rather than an
  unauthenticated door, which was silent. The property that made this urgent is now enforced on
  pahoa's side, which is where it belongs.
- **The console labels the clear operation "Lock slot", not "Remove password"**, since
  `{"password": null}` now bars a slot rather than opening it.
- **The tracker proxy sends `Authorization: Bearer <admin_token>`** on both upstream fetches.
  Puna already proxied server-side, for its own reason — any page whose JavaScript fetches
  `https://mw.ionium-dev.us:41234/api/tracker` puts the room's address in view-source, and the
  tracker link is the one meant for broad sharing. The gate makes that the only thing that works, so
  the two decisions agree.

---

## Constraints puna depends on — do not change these silently

Breaking one of these produces a failure that will not look like it came from pahoa.

1. **`PAHOA_SLOT_PASSWORDS` keeps failing closed.** Puna's Secret builder is now written on the
   assumption that a missing slot is refused. Reverting to fail-open would restore the silent
   open-door failure without any signal that it had.
2. **Passwords must not be persisted into `room.save`.** Fixed in round one and guaranteed by
   regression tests in both directions. The environment is authoritative on every start; a copy on
   disk would shadow the configured value and make rotation appear to work and then revert.
3. **The listener binds only after the save is restored.** Puna's readiness probe means "this room is
   really serving", not "the process started".
4. **A second process on one save directory must keep failing fast.** `save.rs` is the last backstop
   against two pods serving one room, and it is why puna sets `strategy: Recreate` rather than
   trusting its own leader election alone.
5. **Worker threads must keep coming from the cgroup quota**, not `available_parallelism()`. Puna
   always sets `limits.cpu` on the strength of that; if it reverts to host parallelism, a five-slot
   room on a 32-core node spawns 32 workers.
6. **`outbound_budget_for` stays the sizing signal.** Puna derives `limits.memory` from it and the
   seed's slot count — counting **every** slot including spectators and groups, matching
   `slot_info.len()`, not `connectable_slots()`.
7. **Under `--log-format json`, the `serving` event keeps its `message` value and its fields.**
   Restated: this constraint used to name the stdout startup line, and the JSON work correctly
   replaced that line rather than adding to it — puna matches `message == "serving"` now, and its
   docker verification job asserts that stdout is empty and that every stderr line parses. What puna
   depends on is the event, not the channel: renaming it, or moving `addr`/`seed_name` out of it,
   would break the one signal that means "this room is really serving". The banner's `cpu_quota`
   matters for the same reason as constraint 5 — its absence is how a `limits.cpu` that did not apply
   announces itself.
8. **`/healthz` stays unauthenticated and stays on the game port.** It is the kubelet's HTTPS
   readiness and startup probe. Moving it or gating it makes every room unschedulable at once.
9. **`/admin/v1/**` keeps returning `404` when no token is configured**, not `401`. Puna diagnoses a
   Secret that failed to render from exactly that distinction; `401` would read as an ordinary auth
   failure and be retried.
10. **The `429` rate limit keeps its `Retry-After`.** Puna's probe honors it and backs off rather than
    looping, because the limiter locks out the correct token too — deliberately, so it cannot be used
    as an oracle. A reconciler retrying in a tight loop would lock itself out for a minute.
11. **`terminationGracePeriodSeconds: 45`**, per `NOTES-FOR-PUNA.md`'s drain-time analysis. If that
    turns out to be wrong in the cluster the fix is a pahoa flag for `shutdown_timeout`, not a puna
    manifest change — tell us rather than working around it.
12. **The gameplay-option divergence WARN keeps naming the flag, both values and which won.** Puna
    surfaces that line in its room log view rather than filtering it: it is the only signal that a
    pod spec and a running room have parted company, and it is the answer to "I changed the setting
    and nothing happened". A quieter version of it would restore exactly the silence that hid the
    password-persistence bug.
13. **`/admin/v1/status` keeps its `options` block, and keeps the passwords out of it.** Puna renders
    a room's effective gameplay configuration from there, because its own flags describe how a room
    started rather than how it is.

---

## Withdrawn: the `{}` startup error

An earlier revision of this document suggested making `PAHOA_SLOT_PASSWORDS={}` a startup error, on
the grounds that no configuration legitimately wants per-slot mode with zero keys. `secrets.rs`
already answers it, in `merge`: presence of the variable is what turns the mode on, not the map
having entries, and `{}` means *"per-slot mode, nobody holds a key"* — a locked room, and a
deliberate thing to be able to ask for. That is a coherent position and the suggestion is withdrawn.

Puna never emits `{}` regardless. That stays a constraint on puna's side, where it belongs.

---

## P18 — declined, and the rule that came out of it

**Answered: pahoa will not implement a live password setter, and needs nothing from puna.** That was
option 1 of the three ranked here, and the reasoning improved on the question. This document framed
it around the orchestrated/hand-run distinction and the `password_from_env` dead end; the deciding
fact is simpler and larger. **A live password change is wrong in every deployment.** Pahoa persists
no password, so a setter reverts at the next restart whoever ran it — under puna it also disagrees
with the console in the meantime, but those are two severities of one defect rather than two cases.
So there was never a signal to look for, which is why the dead end could not be worked around.

`/option password` and `/option server_password` are refused **by name**, saying they would revert
and where to set them instead, rather than reported as unknown options — recognized and declined are
different facts.

**The rule worth keeping, because it predicts the next case: a setter is honest exactly where the
save is authoritative.** Gameplay options persist through `save::encode_options` and `Room::restore`,
so they now have one. Passwords deliberately do not, so they do not. Two conclusions from one rule
rather than a special case.

Puna's side is unchanged: every `slot_auth` transition was already a restart, and puna sets no
`server_password`, so `!admin` is refused outright in a puna room regardless.

---

## P19 — the data package snapshot, and the one thing puna needs when it goes

**Settled: pahoa will bake `hint_blacklist` into the binary and drop the external
`datapackage.json`.** Recorded here because the investigation behind it is worth not repeating, and
because the removal has one consequence for puna.

### How it surfaced, and whose fault it was

Puna's fault, stated first so the rest reads correctly. Puna passed
`--snapshot=/shared/datapackage.json` on **every** room while **nothing in puna had ever written
that file** — the path existed in three places in puna's tree with no producer anywhere. Every room
in the environment crashlooped. Fixed puna-side: the flag is now emitted only when the file is
actually present, checked per start.

The pahoa-side observation is only this, and it is not a complaint: **`--snapshot` is optional to
*pass* but not optional to *resolve*.** `snapshot: Option<&Path>` reads as "this may be omitted",
which is true, and a caller can slide from there to "this may be absent", which is not. Refusing to
start on a named-but-missing file is correct — silently ignoring it would be worse. Nothing to
change; noted because it is the exact shape of the misreading.

### What the investigation established

Puna asked whether the snapshot was a real reference concept or a pahoa invention. It is real, and
pahoa's own exporter header already said so more precisely than puna's plan did:

- `World.hint_blacklist: ClassVar[FrozenSet[str]]` — `worlds/AutoWorld.py:312`, *"any names that
  should not be hintable"*.
- The reference server loads it from **installed worlds** at `MultiServer.py:344`
  (`self.non_hintable_names[world_name] = world.hint_blacklist`) and enforces it in `!hint` at
  `MultiServer.py:1715` and `:1734` — *`Sorry, "{hint_name}" is marked as non-hintable.`*
- **It is never serialized into multidata by anything.** It is Python class data. The reference
  server can read it only because it *is* an Archipelago install (`import worlds`).
- In the whole reference tree exactly **two** worlds set one: `alttp` → `{"Triforce"}`, and `cvcotm`
  → the Battle Arena reward. Both long-stable.

So the concept must be kept in some form; a standalone Rust server that discarded it would let a
player `!hint Triforce` in an ALttP room, which the reference refuses.

### Why baking it in is the better of the two forms

The external file has three states — present, absent, stale — and pahoa can only distinguish the
first from the other two. Baked-in values cannot be missing, cannot be stale relative to the binary
reading them, and cannot be forgotten by an operator. That last one is not hypothetical: it is how
this was found.

The trade, stated once: the blacklist now tracks the **pahoa build** rather than the apworld version
that generated the seed, so a world *newly* adding a `hint_blacklist` needs a pahoa release to be
honored. Against two stable worlds and an export step nobody performed, that is the cheaper failure.

### The ask — one line, and it is about `SERVE_OPTS`

**When `--snapshot` leaves `SERVE_OPTS`, say so in `NOTES-FOR-PUNA.md`.** Puna transcribes that table
by hand into `PAHOA_SERVE_OPTS` (it cannot import it — the repositories deploy independently), and
an option puna sends that pahoa no longer accepts is a hard `exit 1` on every room at once.

The timing is comfortable and needs no coordination: puna's conditional means it emits no
`--snapshot` today, so a pahoa build without the flag works against the **current** puna image
unchanged. Ship whenever.

The one trap worth both sides knowing: after the flag is gone, a stray `datapackage.json` placed on
puna's shared volume would make puna emit `--snapshot` again — into a parser that now rejects it.
That is this same failure in mirror image, and it looks like adding the right file. Puna will delete
its snapshot machinery and the `/shared` mount in the same commit that re-transcribes `SERVE_OPTS`,
which closes it.

---

## P20 — a fatal error escapes the JSON log stream

**Open, small, and the one thing here that is pahoa's to fix.**

Under `--log-format=json`, `report()` in `crates/pahoa/src/main.rs` prints with
`eprintln!("pahoa: {e}")` (line 449) regardless of the configured format. So a room that dies after the
subscriber is up emits a well-formed JSON banner and then a bare prose line:

```
{"timestamp":"...","level":"INFO","message":"Pahoa-0.1.0-45a5cffa starting","argv":"...",...}
pahoa: /shared/datapackage.json: No such file or directory (os error 2)
```

Both lines are from the same process, after logging is fully initialized. The second is the only one
that says why the room died.

**Why this matters more than a formatting nit**, and all three are things puna is actively building
on:

1. **It breaks the property `--log-format=json` exists to provide.** Pahoa's own reasoning for the
   flag is that a container merges stdout and stderr, so a prose line inside a JSON stream is one
   unparseable entry per room forever. This is that line — and it is the fatal one.
2. **A shipper configured to reject non-JSON drops exactly the diagnosis.** Puna's cluster checklist
   asserts *every* stderr line parses as JSON, precisely so Loki can be configured that way. Under
   that configuration the room's cause of death is the one thing not retained.
3. **Puna's room log view renders fields, not prose.** A line with no `level`, no `message` and no
   `timestamp` either disappears from the view or shows as an unattributed fragment, in the case
   where an organizer most needs an answer.

**Suggested shape, not a specification:** once the subscriber is live, route fatal errors through it
— `tracing::error!` with the message as a field — and keep `eprintln!` only for failures that happen
*before* logging is configured, where it is the sole option. A bad `--log-format` value legitimately
cannot be reported as JSON; a file that failed to open ten lines later can. If that split is awkward,
puna would rather have `eprintln!` retained everywhere and know it than have it fixed halfway, since
"every line is JSON" is checkable and "most lines are JSON" is not.

Not urgent: it costs diagnosability, not correctness, and puna reads pod logs directly today.

---

## P21 — a close that depends on the queue it is closing never happens

**Open, and it produces a half-open connection the client cannot detect.** Found on the dev cluster
2026-08-20, from a symptom an operator reported as "my client thought it was connected to all three
slots, but it wasn't".

`mark_lagged` in `crates/pahoa-net/src/shard.rs`:

```rust
m.lagged = true;
crate::metrics::record_lag_disconnect();
tracing::info!(%conn, "dropping a connection that cannot keep up");
let _ = m.tx.try_send(Outbound::Close("too slow"));
```

A connection is marked lagged **because its outbound queue overflowed**. The close is then
`try_send` onto that same queue, so in the case this exists to handle it fails — and the failure is
discarded by `let _ =`. The room marks the connection lagged, counts a lag disconnect, logs
"dropping a connection", and stops sending to it. **Nothing reaches the socket**, so the peer never
learns and its TCP connection stays open indefinitely.

The result is the worst shape a disconnect can take: the server has forgotten the client, the client
believes it is playing, and neither can tell. It resolves only when the player notices their inputs
have no effect and reconnects by hand.

`ShardMsg::Close` has the same line and the same flaw:

```rust
ShardMsg::Close { conn, reason } => {
    if let Some(m) = members.get(&conn) {
        let _ = m.tx.try_send(Outbound::Close(reason));
    }
}
```

That is the **admin kick** path, so a kick aimed at a slow client — the most likely reason to kick
one — is the case most likely to silently do nothing. On the same cluster a kick reported
`"Disconnected 1 connection for MooingYacht1. They may reconnect."` while the client stayed
connected, which is how this was first noticed.

**The requirement, rather than a prescription: a close must not depend on the queue it is closing.**
Whether that is a reserved slot for control frames, a separate out-of-band signal to the writer
task, or dropping the sender so the writer observes the channel closing and shuts the socket, is
pahoa's call. What puna needs is that a connection pahoa has stopped tracking is a connection the
peer finds out about.

Worth noting the counter is honest either way: `lag_disconnects` counts the *decision*, and puna
re-exports it as `puna_room_lag_disconnects_total`. It read 7 while three sockets were still open —
so the metric is measuring intent, not effect, which is worth a word in its own docs whichever way
this is fixed.

---

## P22 — an admin announcement is indistinguishable from player chat

**Open, small, and two lines.** `admin_say` broadcasts through `broadcast_result`:

```rust
out.broadcast(
    Recipients::AllText,
    &[ServerPacket::PrintJSON(PrintJson {
        data: vec![JsonMessagePart::text(text)],
        print_type: Some(PrintJsonType::CommandResult),
        ..Default::default()
    })],
);
```

Two divergences from the reference, which does this at `MultiServer.py:2233`:

```python
self.ctx.broadcast_text_all('[Server]: ' + raw, {"type": "ServerChat", "message": raw})
```

1. **The type is `CommandResult`, where the reference uses `ServerChat`.** In upstream,
   `CommandResult` is the reply to *your own* command (`notify_client`, `MultiServer.py:1433`);
   `ServerChat` is a server-originated announcement. A client that colors or channels server
   messages will not treat this as one.
2. **There is no `[Server]: ` prefix**, so an announcement arrives as bare unattributed text. On the
   dev cluster an operator sent "Meow?" and could not tell it apart from a player having typed it.

**This belongs to pahoa rather than to its callers**, which is worth stating because the opposite is
arguable. The admin API is deliberately public and puna is one client among several — a token holder
with curl can send an announcement too. If the prefix is the caller's job then every caller must
remember it, they will disagree about the wording, and one that forgets produces a message that
**impersonates a player**. That is a trust property of the room, not a formatting preference, so it
belongs on the side that cannot be bypassed. The `type` can only be set here in any case.

Note the reference puts the prefix in the rendered text *and* the unprefixed original in a `message`
field, so a client can render either. Worth copying whole.

---

## What is not verified

Stated plainly so nobody inherits these as facts.

1. **No generation zip with a `group` slot has been seen.** Across 16 real generation zips — including
   a 96-player, 53-game seed — not one contained an item-link group. Both sides' group handling is
   reasoned from source, not measured, and pahoa's round-two note says the same thing about the
   `/api/tracker` bug it found: it survived review because no zip anyone had contained a group. A
   seed generated with item links enabled would exercise it cheaply and is worth making.
2. **Only one spectator fixture exists**, at
   `~/Downloads/2026-08/output_ecdc2da4-1baa-46c6-812b-b434cd753a05.zip` (3 players + 1 spectator,
   `game="Archipelago"`). Everything about spectators is verified against source on both sides and
   exercised by that one zip, but **no room has actually been run with a spectator connected** — so
   the fail-closed password path and the `connectable_slots` reporting change are both unexercised
   end to end. Worth doing once during puna's M10 cluster checklist.
3. **The acme-dns two-TXT-slot constraint.** Puna's certificate design assumes acme-dns retains only
   the two most recent TXT values per registration, which is why room certs get their own
   registration instead of reusing the gateway's wildcard. Inferred from a warning in
   `int-k8s/apps/ap-lobby-gateway/manifests/gateway.yaml` rather than read from acme-dns's own
   configuration. It affects puna's manifests, not pahoa's code, but it is the kind of inference worth
   flagging before someone relies on it.
