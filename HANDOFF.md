# Handoff: `puna` → `pahoa`

Changes pahoa needs so that **puna** — the Kubernetes room orchestrator and web app at
`/home/troy/src/puna` — can provision, observe and manage pahoa rooms. Earlier revisions cited this
tree by file and line; now that nothing is outstanding, the behavioral detail lives in pahoa's own
`docs/` and in `NOTES-FOR-PUNA.md`, and this document points at them rather than restating them.

Last updated 2026-08-18 against `05be9f5`, during puna's M4 (artifact ingest).

**Nothing is blocking.** P1–P17 are all implemented; the only open item is **P18 below, a design note
about a feature that does not exist yet** and which puna does not need. Otherwise this document is
the record of what was asked and the standing list of what puna depends on — not a queue.
`NOTES-FOR-PUNA.md` is the reply in the other direction and is the authority on what shipped and how;
nothing in it is disputed here.

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

---

## Withdrawn: the `{}` startup error

An earlier revision of this document suggested making `PAHOA_SLOT_PASSWORDS={}` a startup error, on
the grounds that no configuration legitimately wants per-slot mode with zero keys. `secrets.rs`
already answers it, in `merge`: presence of the variable is what turns the mode on, not the map
having entries, and `{}` means *"per-slot mode, nobody holds a key"* — a locked room, and a
deliberate thing to be able to ask for. That is a coherent position and the suggestion is withdrawn.

Puna never emits `{}` regardless. That stays a constraint on puna's side, where it belongs.

---

## P18 — a live password setter, if the `/` command set ever gets one

**Nothing is wrong today.** `!admin` is a stub that masks the echo and refuses, and `cmd_admin`'s own
comment says the `/` command set it would dispatch into "is a later milestone". There is no `/option`
setter anywhere in the tree. This is a note *before* the feature, which is the cheap time to have it.

**The problem it would create.** Pahoa persists no password at all — that was round one's fix, and it
is what makes rotation trustworthy. So a live password change is temporary-until-restart in **every**
deployment, hand-run included. Under puna it is worse than temporary: puna's Secret is the source of
truth, so the change also silently disagrees with what the room console shows, right up until a
restart quietly reverts it. The reference offers `/option password` on its server console, so this is
a real divergence — but the divergence is the right way round, and it follows from not persisting.

**One thing that will look like a clean trigger and is not.** `Secrets::password_from_env` seems like
the natural condition — refuse when the environment supplied the password. It does not work: `pick`
returns `(None, false)` when neither the environment nor argv supplies one, so an **open room reports
`password_from_env: false`**. That is the most common puna room, and it is exactly the `none → room`
transition an organizer would reach for this to perform. There is no capability-shaped fact that
separates "open room, orchestrated" from "open room, run by hand", which is the whole difficulty.

Three coherent positions, ranked:

1. **Do not implement a password setter.** Free, matches today's behavior, and arguably the reference's
   version misleads there too.
2. **Implement it, always labeled as until-restart.** Accurate for every deployment, no coupling to
   anything. The weakness is the audience: the person who reaches for this is an organizer, who
   cannot reach an environment variable and will not know what one is.
3. **Refuse when the room is orchestrated, and say where to go instead.** The best message, and the
   only option that needs anything from puna.

**If option 3, the signal wants three constraints**, all learned from things already settled here:

- **`PAHOA_MANAGED_BY` as free text** (`"Puna"`), not a boolean `PAHOA_UNDER_PUNA`. Same reasoning
  that made `--open-tracker` deployment-shaped rather than puna-shaped: pahoa stays a general tool
  and does not carry one orchestrator's name in its interface.
- **Message-only, never behavior beyond this refusal.** Pahoa varies behavior on *capability* facts
  today — a token is configured, so gate the tracker. An identity fact is a different kind of thing,
  and identity flags accrete conditionals until there are two implementations behind one switch.
- **Assume the value gets said out loud.** `cmd_admin` broadcasts the command echo to the room, and
  error text migrates. So it carries a display name, **never puna's room URL** — that URL is a bearer
  capability, the unguessable path *is* the authorization, which is why `tracker_id` is a separate id
  in the first place.

**Explicitly not in scope: `POST /admin/v1/slots/<n>/password` must stay live.** Not bouncing the room
is its entire purpose. The line is the room-wide password and the mode on one side, per-slot values on
the other.

**Puna needs none of this to work.** Every `slot_auth` transition is a room restart on puna's side
already, for the same non-persistence reason, and puna does not set `server_password`, so `!admin` is
refused outright in a puna room today. This is about what an organizer sees if they try.

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
