# Handoff: `puna` → `pahoa`

Changes pahoa needs so that **puna** — the Kubernetes room orchestrator and web app at
`/home/troy/src/puna` — can provision, observe and manage pahoa rooms. Every claim about current
behavior was read from this tree at **`735a34b`**, with file and line, so it can be confirmed rather
than trusted.

Last updated 2026-08-18, during puna's M4 (artifact ingest).

**Round one is closed.** P1–P15 from the previous revision of this document are all implemented, and
pahoa's reply in `NOTES-FOR-PUNA.md` is the authority on what landed and how — including several
things that came back better than asked for: TLS with hot reload, the filtered second listener,
environment-over-seed secret precedence, and activity timers that survive a restart. That document
has been folded into puna's plan and nothing in it is disputed here.

This round is small and comes from one place: puna's ingest met a real generation zip containing a
**spectator slot**, and following what happens to that slot through pahoa turned up two things worth
deciding deliberately. Neither blocks puna.

---

## What a spectator slot is, and why it turned into two questions

A spectator is an ordinary slot produced by the `Archipelago` pseudo-game (`worlds/generic` in the
reference marks it `SlotType.spectator`). It comes from someone's yaml, it has a name in
`connect_names`, it connects, and it watches the entire multiworld. It simply plays nothing.

**pahoa handles it exactly as upstream does on the path that matters.**
[`room.rs:360`](crates/pahoa-room/src/room.rs#L360) resolves a Connect through
`self.data.connect_names` with no slot-type filter, which is character-for-character what
`MultiServer.py:1880` does. There is no divergence here and nothing to fix on that path.

The two items below are what following the slot *outward* from that point exposed. They are pahoa's
own surfaces, with no upstream analogue to match, so both are design choices rather than bugs
against a reference.

For puna's part: it now stores spectators alongside players and drops only group slots, which is
`WebHostLib/upload.py`'s rule. So a spectator gets an owner, a claim link, a tracker id and — in a
per-slot-password room — a password, like anyone else.

---

## Required changes

| # | Change | Priority |
|---|---|---|
| **P16** | **Reconcile what "a slot" means across the reporting surfaces.** | low — cosmetic today |
| **P17** | **Decide whether `slot_passwords` should fail closed.** | worth settling before the first per-slot room |

### P16 — three surfaces, three different answers

pahoa currently has three distinct notions of which slots exist, and they disagree:

| Surface | Where | What it includes |
|---|---|---|
| Connect / password check | [`room.rs:360`](crates/pahoa-room/src/room.rs#L360) | everything in `connect_names` — **players and spectators** |
| `/admin/v1/status` | [`actor.rs:428`](crates/pahoa-net/src/actor.rs#L428) | `player_slots()` — **players only** |
| `/api/v1/room` | [`http/mod.rs:311`](crates/pahoa-net/src/http/mod.rs#L311) | `player_slots()` — **players only** |
| `/api/tracker` | [`room.rs:2148`](crates/pahoa-room/src/room.rs#L2148) | all of `slot_info` — **players, spectators and groups** |

[`player_slots()`](crates/pahoa-multidata/src/multidata.rs#L277) filters to `SlotType::Player`, and
its doc comment reads *"slots that a client may actually connect to and play"* — which is half
right. A spectator connects; it does not play. The two properties came apart the moment a spectator
appeared, and the name papers over the join.

**The consequence today is small**: puna renders its slot table from its own database, so the only
real loss is that a connected spectator's `connections` count never appears in the console, and
`/api/v1/room` under-reports the room to anyone reading it directly with curl. Nothing breaks.

**A suggested split**, since upstream makes the same distinction and it is a useful guide — its
tracker keeps everything but groups, while `WebHostLib/tracker.py`'s `get_all_players` filters to
`player` for progress-shaped questions:

- **Who may connect** — `/api/v1/room`, `/admin/v1/status`, anything password- or roster-shaped —
  should be `connect_names`-backed: players and spectators, groups excluded.
- **What has been achieved** — check counts, goal status, completion percentages — can stay
  `player_slots()`, because a spectator genuinely has nothing to report there and a `0/0` row is
  noise.

If that split is right, `player_slots()` probably wants a sibling rather than a redefinition, and
the two want names that say which question they answer.

### P17 — `slot_passwords` fails open for a slot it does not know

[`room.rs:371`](crates/pahoa-room/src/room.rs#L371) checks
`self.options.slot_passwords.get(&slot)` and compares it against what the client sent. A slot absent
from that map compares `None` against the offered password — **no password required**. That is
stated deliberately in the code's own comment (*"A slot absent from the map has none"*) and it is
the correct reading of the current contract, which this document's previous revision wrote as *"slots
absent from the object have no password."* So this is a contract question, not an implementation bug.

The concern is the shape of the failure. `PAHOA_SLOT_PASSWORDS` arrives as JSON from an external
system, and [`serve.rs:90`](crates/pahoa/src/serve.rs#L90) assigns it into the room's options with no
cross-check against the seed at all — nothing compares its keys to `connect_names`. So a map that is
merely *incomplete* produces a room where most slots need a credential and one does not, and no
signal anywhere says so. The most likely way to get there is exactly the mistake puna nearly made:
building the map from a slot list that filters out spectators, leaving the one slot that observes
every world in the multiworld as the single unauthenticated door.

`secrets.rs:97` already refuses to start when both password modes are set, which is the right
instinct — a configuration that cannot mean one thing should not run. This is the same class.

Two options, either of which closes it:

1. **Fail closed.** If `slot_passwords` is non-empty, a slot missing from it is refused rather than
   admitted. Simple, and the non-empty map is an unambiguous statement that the room is in per-slot
   mode. The downside is that the failure surfaces as a player who cannot connect, with an
   `InvalidPassword` that is technically accurate and diagnostically useless.
2. **Refuse to start** when `PAHOA_SLOT_PASSWORDS` is set and does not cover every name in
   `connect_names`, naming the missing slots. Louder, earlier, and it matches how pahoa already
   treats a bad `--filtered-port` or a repeated flag: an unstartable room rather than a subtly wrong
   one. Under puna this is a room stuck in `starting` with the reason in the container log, which is
   visible on the room page.

**Preference, weakly held: the second**, possibly both. Puna's map covers every connectable slot as
of today — but that means puna's correctness is currently the only thing standing between a config
slip and an open slot, and the guarantee belongs on the side that enforces it.

If the current behavior is kept deliberately instead, that is a fine answer; it just wants saying out
loud in `docs/`, because "absent means open" and "absent means closed" are both defensible and the
difference is invisible from the outside.

---

## Still open from round one: the race-mode tracker

Carried forward unresolved, and **the severity has dropped** since it was first raised.

[`http/mod.rs:102-103`](crates/pahoa-net/src/http/mod.rs#L102) serves `/api/tracker` and
`/api/static_tracker` with no authentication and `Access-Control-Allow-Origin: *`
([`http/mod.rs:397`](crates/pahoa-net/src/http/mod.rs#L397)), on an internet-reachable port. That is
right for an ordinary room and matches the reference. For a **`race_mode` seed it is not**: anyone
who learns `host:port` can read the full multiworld tracker for a race, and the reference WebHost
restricts race rooms. `race_mode` is parsed
([`multidata.rs:219`](crates/pahoa-multidata/src/multidata.rs#L219)) and exposed to the datastore
([`room.rs:654`](crates/pahoa-room/src/room.rs#L654)), but nothing gates the endpoints on it.

**Why it is less urgent than it was: puna proxies the tracker rather than linking to it.** Contrary
to the suggestion in `NOTES-FOR-PUNA.md` — that puna serve tracker assets and let the browser fetch
the room directly, which the CORS headers were added to support — puna fetches server-side and serves
the document from its own origin. The reason is that any page whose JavaScript fetches
`https://mw.ionium-dev.us:41234/api/tracker` puts the room's address in view-source, and the tracker
link is precisely the one meant for broad sharing. So puna enforces its own `tracker_policy`, and no
puna page discloses `host:port` at all.

That narrows the exposure from "anyone handed a widely-shared tracker link" to "anyone who already
has the room's address" — players, or someone they told. Much smaller, and no longer a leak puna
creates. **The CORS work is not wasted** — it keeps a third-party tracker pointed straight at a room
working, which is worth having.

What remains is genuinely pahoa's, because the endpoint is open regardless of what puna does. The
options are unchanged: gate on the admin token for race seeds, serve a reduced document, or accept it
deliberately and write down why. Puna parses `race_mode` at ingest and stores it, so it can pass the
room's race status in the environment at provisioning time if that is easier than reading it from the
multidata.

---

## Constraints puna depends on — do not change these silently

Carried forward from round one, still load-bearing, with three added from what has shipped since.
Breaking one produces a failure that will not look like it came from pahoa.

1. **Passwords must not be persisted into `room.save`.** Fixed in round one and now guaranteed by
   regression tests in both directions. The environment is authoritative on every start; a copy on
   disk would shadow the configured value and make rotation appear to work and then revert.
2. **The listener binds only after the save is restored.** Puna's readiness probe means "this room is
   really serving", not "the process started".
3. **A second process on one save directory must keep failing fast.** `save.rs` is the last backstop
   against two pods serving one room, and it is why puna sets `strategy: Recreate` rather than
   trusting its own leader election alone.
4. **Worker threads must keep coming from the cgroup quota**, not `available_parallelism()`. Puna
   always sets `limits.cpu` on the strength of that; if it reverts to host parallelism, a five-slot
   room on a 32-core node spawns 32 workers.
5. **`outbound_budget_for` stays the sizing signal.** Puna derives `limits.memory` from it and the
   seed's slot count — counting **every** slot including spectators and groups, matching
   `slot_info.len()`.
6. **The startup line keeps its shape**, and now carries the build version (P9).
7. **`/healthz` stays unauthenticated and stays on the game port.** It is the kubelet's HTTPS
   readiness and startup probe. Moving it or gating it makes every room unschedulable at once.
8. **`/admin/v1/**` keeps returning `404` when no token is configured**, not `401`. Puna diagnoses a
   Secret that failed to render from exactly that distinction; `401` would read as an ordinary auth
   failure and be retried.
9. **The `429` rate limit keeps its `Retry-After`.** Puna's probe honors it and backs off rather than
   looping, because the limiter locks out the correct token too — deliberately, so it cannot be used
   as an oracle. A reconciler retrying in a tight loop would lock itself out for a minute.

---

## What is not verified

Stated plainly so nobody inherits these as facts.

1. **No generation zip with a `group` slot has been seen.** Puna drops groups at ingest and pahoa's
   tracker emits them separately via `group_members`, but across 16 real generation zips — including
   a 96-player, 53-game seed — not one contained an item-link group. Both sides' group handling is
   reasoned from source, not measured. A seed with item links enabled would exercise it cheaply and
   is worth generating.
2. **Only one spectator fixture exists**, at
   `~/Downloads/2026-08/output_ecdc2da4-1baa-46c6-812b-b434cd753a05.zip` (3 players + 1 spectator,
   `game="Archipelago"`). Everything above about spectators is verified against source on both sides
   and exercised by that one zip; no room has actually been run with a spectator connected, so P16's
   practical impact is inferred rather than observed.
3. **The acme-dns two-TXT-slot constraint.** Puna's certificate design assumes acme-dns retains only
   the two most recent TXT values per registration, which is why room certs get their own
   registration instead of reusing the gateway's wildcard. Inferred from a warning in
   `int-k8s/apps/ap-lobby-gateway/manifests/gateway.yaml` rather than read from acme-dns's own
   configuration. It affects puna's manifests, not pahoa's code, but it is the kind of inference worth
   flagging before someone relies on it.
