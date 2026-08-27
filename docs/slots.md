# What "a slot" means, and which question a surface is asking

A seed's `slot_info` holds three kinds of slot, and they are not interchangeable:

- **Player** — someone with a world, checks to make and a goal to reach.
- **Spectator** — an ordinary slot produced by the `Archipelago` pseudo-game. It comes from
  someone's yaml, has a name in `connect_names`, connects, and watches the entire multiworld. It
  simply plays nothing.
- **Group** — an item-link construct. No client ever connects as one.

For a long time only players existed in practice, so "a slot" was unambiguous and one accessor
served every purpose. A spectator pulls two properties apart that had always traveled together:
**who may connect**, and **who has progress to report**. Using one where the other belongs is how a
spectator goes missing from a roster, or turns up as a permanently idle player.

## The two accessors

| | Includes | For |
|---|---|---|
| `MultiData::connectable_slots` | players + spectators | roster questions |
| `MultiData::player_slots` | players only | progress questions |

**Roster questions** are anything password-, presence- or membership-shaped: who needs a
credential, who appears on a room page, whose connections are counted. A spectator is a participant
and belongs in all of them. Groups never do — nothing connects as one. This matches
`WebHostLib/upload.py`, which keeps everything but groups.

**Progress questions** are check counts, goal status, completion percentages. A spectator has
nothing to report and would be a `0/0` row, which is noise rather than information.

## Where each is used

| Surface | Question | Set |
|---|---|---|
| Connect / password check | roster | `connect_names`, which is the same membership |
| `GET /api/v1/room` | roster | `connectable_slots` |
| `GET /admin/v1/status` | roster | `connectable_slots` |
| `/api/tracker` per-player arrays | progress | players only |
| `/api/tracker` `hints` | roster-ish | every slot |
| `/api/static_tracker` `groups` | — | groups only |

The tracker's split is not pahoa's choice: it mirrors the reference, which walks `get_all_players()`
for the per-player arrays and `get_all_slots()` for hints alone. Hints span everything because a
hint concerns a *pair* of slots and either end may be a group.

## The rule for anything new

Ask which question the surface answers before picking an accessor. If it is about **who is in the
room**, use `connectable_slots`. If it is about **what has been achieved**, use `player_slots`. If
neither reading is obviously right, that is a sign the surface is answering two questions and wants
splitting rather than a coin toss.
