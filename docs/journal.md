# The room journal

`--journal` appends one JSON line per location checked to `history.jsonl` in the save directory, for
as long as the room exists and across every restart of it.

```json
{"type":"check","at":1787157141.420,"finder":1,"finder_name":"amperketBalala",
 "receiver":1,"receiver_name":"amperketBalala","item":5606235,"item_name":"Archipelago Tarot",
 "location":5606192,"location_name":"Green Deck Ante 1 White Stake","flags":1}
```

It is off by default and needs `--save-dir`, since the history is kept beside the save so that it
survives a restart the way the save does.

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

## Durability

The writer flushes every 1024 records and on the save timer, so the journal and the save agree about
how much a hard kill can cost. An `fsync` per check would make a release disk-bound for a guarantee
nobody asked for: the save file makes the same bargain for the same reason.

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

Only checks. A cheat-console grant, a collect, a goal and an admin action are all things an organizer
might reasonably want in the same file, and the `type` field exists so they can be added without
changing what a reader does with the lines it already understands. Checks came first because they are
what a release produces in bulk, and therefore the case that had to be proven cheap.
