#!/usr/bin/env python3
"""Generate `!hint` selection vectors from Archipelago's own `get_hints`.

This is M6's exit gate. Hint *ordering* cannot be matched — see the note below
— but everything that decides **which** hints a player gets, and what they pay
for them, can be, and this drives the reference implementation to find out.

The context is a real `MultiServer.Context` with only `_load_game_data`
overridden, exactly as `WebHostLib.customserver.WebHostContext` does it: the
data package comes from the multidata instead of from importing `worlds`. So
`collect_hints`, `get_sphere`, `get_hint_cost`, `get_client_points` and the
selection block inside `get_hints` are all the genuine articles.

## What is and is not reproducible

`get_hints` splits its candidates into `set(hints) - ctx.hints[...]` and then
shuffles the result. Set iteration order depends on `Hint.__hash__`, which
includes the `entrance` string, and CPython randomizes string hashing per
process — so for an entrance-randomized seed the reference does not even agree
with itself between restarts. What *is* stable:

- the candidate set (`collect_hints` / `collect_hint_location_id`)
- each candidate's sphere, and whether it is a local placement
- the ordering *rule*: the granted hint always comes from the lowest sphere,
  preferring a non-local placement
- how many hints one call grants, and what it costs

So the vectors record the candidates with their sort keys, the granted hints,
the points arithmetic and the exact reply text. Ordering within the winning
group is left alone.

Generated under `PYTHONHASHSEED=0` for a reproducible run; nothing downstream
depends on that.

    PYTHONHASHSEED=0 ~/src/Archipelago/.venv/bin/python tools/gen-hint-vectors.py \\
        --archipelago ~/src/Archipelago \\
        --multidata crates/pahoa-pickle/tests/fixtures/AP_56807069331869547085.archipelago \\
        > crates/pahoa-room/tests/hint_vectors.jsonl
"""

import argparse
import json
import logging
import sys

EMBEDDED_PACKAGE = {}


def build_context(archipelago, multidata, hint_cost, check_points):
    import MultiServer

    class Headless(MultiServer.Context):
        """`WebHostContext`'s trick: never import the world system.

        `Context._load_game_data` does `import worlds`; the room process in
        production asserts that module is *absent* (`customserver.py:360-361`),
        so overriding it is what the reference itself does rather than a
        shortcut taken here.
        """

        def _load_game_data(self):
            self.gamespackage = EMBEDDED_PACKAGE
            self.item_name_groups = {}
            self.location_name_groups = {}

    ctx = Headless(
        "",
        0,
        None,
        None,
        check_points,
        hint_cost,
        True,
        logger=logging.getLogger("gen-hint-vectors"),
    )
    ctx.load(multidata)
    return ctx


class FakeClient:
    """Enough of `MultiServer.Client` for the hint path."""

    def __init__(self, team, slot):
        self.team = team
        self.slot = slot
        self.auth = True
        self.no_text = False
        self.tags = ["AP"]
        self.version = (0, 6, 8)


def hint_json(h):
    return {
        "receiving_player": h.receiving_player,
        "finding_player": h.finding_player,
        "location": h.location,
        "item": h.item,
        "found": h.found,
        "entrance": h.entrance,
        "item_flags": h.item_flags,
        "status": int(h.status),
    }


def run_case(ctx, MultiServer, slot, text, for_location, checks):
    """Run one `!hint` and report everything stable about the outcome."""
    team = 0
    # Fresh state per case: the point of a vector is that it stands alone.
    ctx.location_checks[team, slot] = set(checks)
    ctx.hints[team, slot] = set()
    ctx.hints_used[team, slot] = 0

    client = FakeClient(team, slot)
    processor = MultiServer.ClientMessageProcessor(ctx, client)

    outputs = []
    processor.output = outputs.append
    processor.output_multiple = outputs.extend

    announced = []
    real_notify = ctx.notify_hints

    def spy_notify(team_, hints, **kwargs):
        # Wrapped, not replaced: the real `notify_hints` decides what actually
        # gets *stored*, and its guard looks at the finding player's list — not
        # the hinting slot's — so a placement the seed already hinted is
        # announced but not banked again. Reimplementing that here would be
        # exactly the transcription error these vectors exist to rule out.
        announced.extend(hints)
        return real_notify(team_, hints, **kwargs)

    ctx.notify_hints = spy_notify
    # No event loop here, so the two paths that would schedule a send are cut.
    # Every slot's client list is empty, so `notify_hints` skips its own sends.
    ctx.broadcast = lambda endpoints, msgs: None
    ctx.save = lambda now=False: False

    points_before = MultiServer.get_client_points(ctx, client)
    cost = ctx.get_hint_cost(slot)

    # The candidate pool, resolved the same way `get_hints` resolves it, so the
    # vector records what the selection was choosing *from*.
    candidates = resolve_candidates(ctx, MultiServer, team, slot, text, for_location)

    processor.get_hints(text, for_location)

    # Announced and stored are not the same list. A hint the seed had already
    # placed in the *finding* player's list is announced and paid for, but
    # `notify_hints` will not bank a second copy — so both are recorded.
    free = [h for h in announced if h.found]
    granted = [h for h in announced if not h.found]
    stored = list(ctx.hints[team, slot])
    return {
        "slot": slot,
        "input": text,
        "for_location": for_location,
        "checked": sorted(checks),
        "hint_cost_percent": ctx.hint_cost,
        "location_check_points": ctx.location_check_points,
        "cost": cost,
        "points_before": points_before,
        "points_after": MultiServer.get_client_points(ctx, client),
        "hints_used": ctx.hints_used[team, slot],
        "candidates": [
            {
                **hint_json(h),
                "sphere": ctx.get_sphere(h.finding_player, h.location),
                "local": h.local,
            }
            for h in candidates
        ],
        "granted": sorted(
            (hint_json(h) for h in granted),
            key=lambda d: (d["finding_player"], d["location"]),
        ),
        "free": sorted(
            (hint_json(h) for h in free),
            key=lambda d: (d["finding_player"], d["location"]),
        ),
        "stored": sorted(
            (hint_json(h) for h in stored),
            key=lambda d: (d["finding_player"], d["location"]),
        ),
        "output": outputs,
        "suggestion_is_tied": suggestion_is_tied(ctx, text, for_location, slot),
    }


def resolve_candidates(ctx, MultiServer, team, slot, text, for_location):
    """The pool `get_hints` would build, without the payment step.

    Mirrors the dispatch in `MultiServer.py:1707-1755` — id, group name, or
    plain name — using the reference's own collectors for each branch.
    """
    if not text:
        return []
    game = ctx.games[slot]
    if text.isnumeric():
        hint_id = int(text)
        if for_location:
            return MultiServer.collect_hint_location_id(ctx, team, slot, hint_id)
        return MultiServer.collect_hints(ctx, team, slot, hint_id)

    names = (
        ctx.all_location_and_group_names[game]
        if for_location
        else ctx.all_item_and_group_names[game]
    )
    from Utils import get_intended_text

    name, usable, _ = get_intended_text(text, names)
    if not usable:
        return []
    if not for_location and name in ctx.item_name_groups.get(game, {}):
        out = []
        for item_name in ctx.item_name_groups[game][name]:
            if item_name in ctx.item_names_for_game(game):
                out.extend(MultiServer.collect_hints(ctx, team, slot, item_name))
        return out
    if not for_location and name in ctx.item_names_for_game(game):
        return MultiServer.collect_hints(ctx, team, slot, name)
    if name in ctx.location_name_groups.get(game, {}):
        out = []
        for loc_name in ctx.location_name_groups[game][name]:
            if loc_name in ctx.location_names_for_game(game):
                out.extend(
                    MultiServer.collect_hint_location_name(ctx, team, slot, loc_name)
                )
        return out
    return MultiServer.collect_hint_location_name(ctx, team, slot, name)


def suggestion_is_tied(ctx, text, for_location, slot):
    """Whether the "did you mean …" name this run produced is reproducible.

    `!hint` feeds the fuzzy matcher `all_item_and_group_names[game]`, which is a
    **`set`** (`MultiServer.py:248`). Set iteration order for strings follows
    `PYTHONHASHSEED`, which CPython randomizes per process — so when several
    candidates share the top score, *which* one the reference names is an
    artifact of the run rather than a property of Archipelago. Generating the
    same seed under four hash seeds gives four different suggestions.

    Recording that here is what lets the Rust side compare the reply exactly
    where it can, and only elide the name where the reference does not agree
    with itself. The same reasoning as hint ordering, which this file has always
    declined to compare.
    """
    from Utils import get_fuzzy_results

    game = ctx.games[slot]
    names = (
        ctx.all_location_and_group_names.get(game)
        if for_location
        else ctx.all_item_and_group_names.get(game)
    )
    if not names or not text:
        return False
    picks = get_fuzzy_results(text, names)
    if not picks:
        return False
    return sum(1 for _, score in picks if score == picks[0][1]) > 1


def pick_scenarios(ctx):
    """Choose slots and item names that exercise the interesting branches.

    Derived from the multidata rather than hard-coded, so the same script works
    against any fixture — but reported in the vector, so the Rust side drives
    the identical case.
    """
    import collections

    from NetUtils import SlotType

    scenarios = []
    # Compared against the enum rather than `str(...)`: `SlotType` is an
    # `IntEnum`, and since Python 3.11 that stringifies as "1", not
    # "SlotType.player". The `or` fallback below hid that — every slot in the
    # current fixture is a player, so the vectors were unaffected, but on a seed
    # with spectators or item-link groups it would have picked one of those.
    player_slots = [
        s for s, info in ctx.slot_info.items() if info.type == SlotType.player
    ] or list(ctx.slot_info)

    # A slot with no entrance data: its hints hash deterministically, so even
    # the ordering is stable there. Preferred, because it makes the vector a
    # tighter test.
    plain = [s for s in player_slots if not ctx.er_hint_data.get(s)] or player_slots

    # How many placements each slot is owed, which is what decides whether the
    # scenarios below have anything to choose between.
    owed_by_slot = collections.defaultdict(collections.Counter)
    for finder, locs in ctx.locations.items():
        for loc, (item, receiver, _flags) in locs.items():
            owed_by_slot[receiver][item] += 1

    # The *richest* eligible slot, not merely the first. Taking `[0]` was fine
    # while every fixture's slot 1 happened to be well supplied; on a seed where
    # it is owed two items the vectors degenerate to empty candidate sets and
    # stop testing the selection rule at all. Ties break on the lowest slot so
    # the choice stays deterministic.
    subject = max(plain, key=lambda s: (sum(owed_by_slot[s].values()), -s))
    owed = owed_by_slot[subject]
    names = ctx.item_names_for_game(ctx.games[subject])
    by_id = {i: n for n, i in names.items()}
    ranked = [i for i, _ in owed.most_common() if i in by_id]
    multi = by_id[ranked[0]] if ranked else None
    single = by_id[ranked[-1]] if ranked else None

    own_locations = sorted(ctx.locations[subject])
    loc_names = ctx.location_names_for_game(ctx.games[subject])
    loc_by_id = {i: n for n, i in loc_names.items()}
    a_location = next((loc_by_id[l] for l in own_locations if l in loc_by_id), None)

    if multi:
        # Free hints: everything the pool holds, in one call.
        scenarios.append((subject, multi, False, [], 0, 1))
        # Priced, and affordable exactly once.
        scenarios.append((subject, multi, False, own_locations[:20], 10, 1))
        # Priced and unaffordable.
        scenarios.append((subject, multi, False, [], 10, 1))
    if single:
        scenarios.append((subject, single, False, [], 0, 1))
    if a_location:
        scenarios.append((subject, a_location, True, [], 0, 1))
        # The same location, already checked: found hints are free.
        checked = [l for l, n in loc_by_id.items() if n == a_location]
        scenarios.append((subject, a_location, True, checked, 10, 1))
    # A name nothing matches, and the empty input that just quotes the price.
    scenarios.append((subject, "zzzzzzzzzzzzzzzz", False, [], 10, 1))
    scenarios.append((subject, "", False, [], 10, 1))
    # An id rather than a name.
    if ranked:
        scenarios.append((subject, str(ranked[0]), False, [], 0, 1))
    return scenarios


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    ap.add_argument("--multidata", required=True)
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]
    import ModuleUpdate

    ModuleUpdate.update_ran = True
    import MultiServer

    # One context to choose the cases, then one per case so hint_cost and
    # location_check_points can vary without carrying state across.
    probe = build_context(args.archipelago, args.multidata, 10, 1)
    scenarios = pick_scenarios(probe)

    emitted = 0
    for slot, text, for_location, checks, cost, points in scenarios:
        ctx = build_context(args.archipelago, args.multidata, cost, points)
        case = run_case(ctx, MultiServer, slot, text, for_location, checks)
        case["seed_name"] = ctx.seed_name
        print(json.dumps(case))
        emitted += 1

    print(f"emitted {emitted} hint vectors", file=sys.stderr)


if __name__ == "__main__":
    main()
