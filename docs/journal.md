# The room journal

`--journal` appends the room's history to `history.jsonl` in the save directory, one JSON line per
event, for as long as the room exists and across every restart of it. It is off by default and needs
`--save-dir`, since the history is kept beside the save so that it survives a restart the way the
save does.

Every line has a `type`, and a reader is expected to dispatch on it and ignore what it does not know.

| `type` | when | notable fields |
|---|---|---|
| `started` | a room process began serving | `version`, `build_rev` |
| `stopped` | it stopped cleanly | `reason`, `version`, `build_rev` |
| `check` | a location became checked, including via release and collect | `finder`, `receiver`, `item_name`, `location_name`, `flags` |
| `cheat` | `!getitem` conjured an item | `slot`, `item_name` |
| `hints` | hints were granted | `granted`, `cost`, `points_before`, `points_after` |
| `chat` | anything said in the room | `slot`, `text` |
| `deathlink` | a `Bounce` tagged DeathLink | `slot`, `cause`, `source`, `recipients` |
| `options` | room start, and after any option change | every option, plus `password_mode` |
| `option_changed` | `!admin /option` | `option`, `value` |
| `slot_password_changed` | the admin API set or cleared one | `slot`, `set` |
| `gap` | the writer had to drop records | `dropped` |

```json
{"type":"started","at":1787844931.943,"version":"0.1.0","build_rev":"497b5e1"}
{"type":"stopped","at":1787845029.761,"reason":"SIGTERM","version":"0.1.0","build_rev":"497b5e1"}
{"type":"check","at":1787157141.420,"finder":1,"finder_name":"amperketBalala",
 "receiver":1,"receiver_name":"amperketBalala","item":5606235,"item_name":"Archipelago Tarot",
 "location":5606192,"location_name":"Green Deck Ante 1 White Stake","flags":1}
{"type":"cheat","at":1787159857.903,"team":0,"slot":1,"player":"amperketBalala",
 "item":5606235,"item_name":"Archipelago Tarot"}
{"type":"hints","at":1787159961.659,"team":0,"slot":1,"player":"amperketBalala",
 "granted":["amperketBalala's Archipelago Tarot at Green Deck Ante 1 White Stake (amperketBalala)"],
 "cost":6,"points_before":200,"points_after":194}
{"type":"deathlink","at":1787159859.507,"team":0,"slot":1,"player":"amperketBalala",
 "cause":"fell in a pit","source":"amperketBalala","recipients":1}
```

## What it deliberately does not contain

**No password, ever.** Two paths could have put one here and both are closed:

- `chat` is built from the text the room *broadcast*, which for `!admin` is already masked by
  `cmd_admin` before anything else sees it. So `!admin login hunter2` is journalled as
  `!admin login ****************`. Recording anything earlier in that path would undo the masking
  into a file that outlives the room, which is the worst place for a password to reappear.
- `options` and `slot_password_changed` carry password **modes and facts**, never values:
  `password_mode` is `none` / `room` / `per-slot`, `server_password_set` is a boolean, and a slot
  password change records only whether one was set. Clearing a slot password *locks* that slot rather
  than opening it, so `set: false` is the more consequential of the two and is the record that
  answers "why can nobody join slot 4" months later.

**Only DeathLink among bounces.** `Bounce` is a general relay that forks and trackers use for their
own traffic, and unlike checks its volume is unbounded — checks are capped by the seed's location
count, deaths are not. Journalling all of it would let one chatty client dominate a room's history.

## Why the option set is written twice

`option_changed` says what moved; the `options` line that follows says what the rules now are. The
redundancy is the point: without it, reconstructing the room's configuration at any moment means
replaying every change from the beginning and hoping none were dropped. `options` is also written at
every start — not only the first — because a restart is exactly when the configuration can have
changed underneath the room.

## Who it is for

**The organizer of one room**, and that audience is the whole design. The same events could be
logged to stderr and shipped to a log aggregator, which is the right answer for an *operator*
debugging across rooms — but it is the wrong one here, on three counts that are about access rather
than durability.

- **Authorization.** Loki isolates by tenant and has no label-level access control, so "this
  organizer may read this room and nothing else" is not something the store enforces. Scoping would
  mean routing room logs to their own tenant, which is shipper configuration owned by the platform.
  A file in the room's own directory needs none of that, and a mistake exposes one room's own data
  rather than the platform's logs.
- **Lifetime.** Retention is a platform setting. An async room outliving that window loses the
  history the organizer wanted, and nobody finds out until they ask for it.
- **Identity across restarts.** Pod logs are labelled by pod, and a restarted room is a new pod;
  reassembling one room's history means promoting a stable label through the shipper. The save
  directory is the same directory by definition, so appending to a file in it is continuous for
  free.

None of this argues against also shipping logs. The two are one event stream for two consumers with
different lifetimes and different authorization stories — **Loki for operators, the journal for the
room**. What the journal deliberately is *not* is a second copy of the operational log: nothing about
checks goes to stderr.

## Why it is threaded

`release_player` feeds every location a slot owns through the check path in one burst. The numbers
that decided the shape, measured on a 2000-slot seed with 341,851 locations:

| | cost | share of the release |
|---|---|---|
| the release itself, no journal | 283 ms | — |
| formatting those checks as JSON inline | 418 ms | 148% |
| formatting **and** writing to a drained pipe | 809 ms | 286% |
| writing to a *stalled* consumer | did not finish in 12 s | unbounded |
| **what pahoa does: `try_send` a `Copy` record** | **2.5 ms** | **0.9%** |

So the actor pushes plain numbers into a bounded channel and a thread does everything else — name
resolution, JSON, the write. Two consequences worth stating:

- **[`CheckRecord`](../crates/pahoa-room/src/effect.rs) carries ids, not names.** Resolving four
  names per record would put the allocations back on the task that owns all room state. The writer
  holds the `MultiData` and the name tables and resolves on its own time.
- **The channel drops rather than blocks when full.** The alternative is letting a stalled disk stop
  a live multiworld, and the last row above is what that looks like. A drop is never silent: the
  count is written *into the journal*, as a `{"type":"gap","dropped":n}` line, because the journal is
  the artifact somebody reads later and a warning in a log stream this room may not be shipping is
  not good enough.

The buffer holds 2<sup>19</sup> records, which is more than the largest burst a single release can
produce on any real seed — so the drop path is reserved for a disk that has genuinely stopped, not
for ordinary play.

## Incarnations, and telling a crash from a quiet night

The file spans every run of a room — that is the whole reason it lives beside the save rather than in
the log stream — so `started` and `stopped` are what divide it into the runs that produced it. Each
carries the `version` and the git `build_rev`, which is what makes "did this room's behavior change
under it" answerable months later, when the version number alone has been reused by a dozen builds.

**A `started` with no `stopped` before it is an unclean stop.** That is the design rather than a gap
in it: a process killed outright — `SIGKILL`, an OOM kill, a node disappearing — writes nothing,
because there is nothing left that could write it. So the absence is the signal, and it is legible to
somebody who never saw the pod. Writing a closing record optimistically at startup was the obvious
alternative and would state the opposite of the truth in exactly the case worth detecting.

`reason` is the same word the shutdown log line uses, so the two match without a translation table:
`SIGTERM` for an orchestrator draining a pod, `SIGINT` for a person at a terminal, and
`admin request` for `POST /admin/v1/shutdown`.

## Durability

The writer flushes every 1024 records, after one second of quiet, and on the save timer — so the
journal and the save agree about how much a hard kill can cost. An `fsync` per check would make a
release disk-bound for a guarantee nobody asked for: the save file makes the same bargain for the
same reason.

The idle second matters because a count alone scales the wrong way for a reader. A busy room passes
1024 constantly and is always fresh; a quiet room would reach the disk only on the save tick, so the
room with somebody watching the feed was the room whose file was worst.

**`started` is flushed the moment it is written**, ahead of any of that. It is the marker that says
the previous incarnation never stopped, so a room that dies in its first second — which is when a bad
config, a wedged mount or an OOM kill takes one — has to have already left the evidence.

At shutdown the actor's handle is dropped, which closes the channel, and the process joins the writer
before exiting — so a clean stop never leaves records in a buffer.

## Size

One line is around 264 bytes. A full playthrough is therefore roughly:

| seed | checks | journal |
|---|---|---|
| 96-slot | 23,404 | 6.2 MB |
| largest real seed seen | 120,027 | 31.7 MB |
| 2000-slot | 341,851 | 90.2 MB |

Nothing that matters on the network filesystem a save already lives on, and unlike a log stream it is
not competing with anyone else's retention.

## What it does not yet record

Goal completions, joins and parts, countdowns, and admin actions other than option and slot-password
changes. The `type` field exists so these can be added without changing what a reader does with the
lines it already understands.

Two of the events above are worth a note on why they are shaped as they are:

- **`hints` carries both balances, not just the cost.** Hint price is a percentage of a slot's own
  location count and can be changed mid-room with `!admin /option hint_cost`, so a cost recorded in
  isolation cannot be checked against anything afterwards. `points_before` and `points_after` can. A
  hint for an item at an already-checked location is free, and shows up as the two being equal —
  which is the distinction an organizer is usually being asked to adjudicate.
- **`cheat` exists because no `check` can account for it.** `!getitem` moves an item with no location
  behind it, so without this line the history reads as a complete account of where every item came
  from and quietly is not. `item_cheat` defaults to *on*, so this is reachable in any room that has
  not turned it off.
