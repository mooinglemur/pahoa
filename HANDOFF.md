# Handoff: `puna` → `pahoa`

Changes pahoa needs so that **puna** — the Kubernetes room orchestrator and web app at
`/home/troy/src/puna` — can provision, observe and manage pahoa rooms. Earlier revisions cited this
tree by file and line; now that nothing is outstanding, the behavioral detail lives in pahoa's own
`docs/` and in `NOTES-FOR-PUNA.md`, and this document points at them rather than restating them.

Last updated 2026-08-18 against `481ddb8`, at the end of puna's M4 (artifact ingest and the gates).

**Nothing is outstanding.** P1–P18 are all resolved — P18 by being declined, which was the right
answer and the first of the three options this document ranked. This is now the record of what was
asked and the standing list of what puna depends on, not a queue. `NOTES-FOR-PUNA.md` is the reply in
the other direction and is the authority on what shipped and how; nothing in it is disputed here.

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
7. **The startup line keeps its shape**, and now carries the build version (P9).
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
