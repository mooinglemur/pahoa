#!/usr/bin/env python3
"""Synthesise a large `.archipelago` for scale testing.

pahoa targets ~2000 slots and 6000+ concurrent connections, but no seed that
size exists to test against — generating a real one would mean running
Archipelago generation over 2000 yamls. This builds a structurally valid
multidata at that scale instead.

It is explicitly **synthetic**: the item and location ids are drawn from a real
seed's data package so names resolve, but the placement is mechanical rather
than the output of a fill algorithm. That makes it right for what it is for —
parse cost, LocationStore behaviour, memory footprint, broadcast fan-out — and
wrong for anything about game logic or reachability.

Usage:
    make-large-fixture.py --template <real.archipelago> --slots 2000 \\
        --out large.archipelago
"""

import argparse
import hashlib
import io
import pickle
import random
import zlib

ALLOWED = {
    ("NetUtils", "NetworkItem"),
    ("NetUtils", "NetworkSlot"),
    ("NetUtils", "Hint"),
    ("NetUtils", "SlotType"),
    ("NetUtils", "HintStatus"),
    ("NetUtils", "ClientStatus"),
    ("collections", "Counter"),
}


class Instance:
    """Mirrors PyObj::Instance: class identity plus positional args."""

    __slots__ = ("cls", "args")

    def __init__(self, cls, args):
        self.cls = cls
        self.args = args


def _stub(module, name):
    class Stub:
        def __new__(cls, *args):
            return Instance(f"{module}.{name}", list(args))

    Stub.__name__ = name
    return Stub


class StubUnpickler(pickle.Unpickler):
    def find_class(self, module, name):
        if (module, name) in ALLOWED:
            return _stub(module, name)
        raise pickle.UnpicklingError(f"global '{module}.{name}' is forbidden")


# Writing side: real namedtuples/enums so the output pickles exactly as
# Archipelago's would, using the same NEWOBJ / REDUCE opcode shapes.
import collections  # noqa: E402
import enum  # noqa: E402


class ByValue:
    """Archipelago's mixin: pickle enums by value, not by name (Utils.py:502)."""

    def __reduce__(self):
        return (self.__class__, (self._value_,))


class SlotType(ByValue, enum.IntFlag):
    spectator = 0b00
    player = 0b01
    group = 0b10


class HintStatus(ByValue, enum.IntEnum):
    HINT_UNSPECIFIED = 0
    HINT_NO_PRIORITY = 10
    HINT_AVOID = 20
    HINT_PRIORITY = 30
    HINT_FOUND = 40


NetworkSlot = collections.namedtuple(
    "NetworkSlot", ["name", "game", "type", "group_members"]
)
Hint = collections.namedtuple(
    "Hint",
    [
        "receiving_player",
        "finding_player",
        "location",
        "item",
        "found",
        "entrance",
        "item_flags",
        "status",
    ],
)

# Make them pickle under NetUtils, matching a real multidata. Setting
# __module__ alone is not enough: pickle imports the named module to verify the
# class round-trips, so a stand-in has to exist in sys.modules. Nothing here
# imports Archipelago — this is a shim that exists only to emit the right
# STACK_GLOBAL names.
import sys  # noqa: E402
import types  # noqa: E402

_netutils = types.ModuleType("NetUtils")
for cls in (SlotType, HintStatus, NetworkSlot, Hint):
    cls.__module__ = "NetUtils"
    setattr(_netutils, cls.__name__, cls)
sys.modules.setdefault("NetUtils", _netutils)


def load_template(path):
    with open(path, "rb") as f:
        data = f.read()
    return StubUnpickler(io.BytesIO(zlib.decompress(data[1:]))).load()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--template", required=True, help="a real .archipelago to borrow games from")
    ap.add_argument("--slots", type=int, default=2000)
    ap.add_argument("--locations-per-slot", type=int, default=200)
    ap.add_argument("--groups", type=int, default=8, help="item-link group slots")
    ap.add_argument("--spectators", type=int, default=4)
    ap.add_argument("--seed", type=int, default=1234, help="RNG seed, for reproducibility")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    template = load_template(args.template)

    # Borrow the real data package so names resolve and checksums are genuine.
    datapackage = template.get("datapackage", {})
    games = sorted(g for g in datapackage if g != "Archipelago")
    if not games:
        raise SystemExit("template has no embedded data package to borrow")

    per_game_items = {}
    per_game_locs = {}
    for g in games:
        items = sorted(datapackage[g].get("item_name_to_id", {}).values())
        locs = sorted(datapackage[g].get("location_name_to_id", {}).values())
        if items and locs:
            per_game_items[g] = items
            per_game_locs[g] = locs
    games = [g for g in games if g in per_game_items]

    n_players = args.slots - args.groups - args.spectators
    if n_players < 1:
        raise SystemExit("--slots too small for the requested groups/spectators")

    slot_info = {}
    connect_names = {}
    slot_games = {}

    slot = 1
    for i in range(n_players):
        game = games[i % len(games)]
        name = f"Player{i + 1}"
        slot_info[slot] = NetworkSlot(name, game, SlotType.player, ())
        connect_names[name] = (0, slot)
        slot_games[slot] = game
        slot += 1

    for i in range(args.spectators):
        name = f"Spectator{i + 1}"
        slot_info[slot] = NetworkSlot(name, "Archipelago", SlotType.spectator, ())
        connect_names[name] = (0, slot)
        slot += 1

    player_slots = [s for s, info in slot_info.items() if info.type == SlotType.player]
    for i in range(args.groups):
        members = tuple(sorted(rng.sample(player_slots, min(6, len(player_slots)))))
        name = f"ItemLink{i + 1}"
        slot_info[slot] = NetworkSlot(name, "Archipelago", SlotType.group, members)
        connect_names[name] = (0, slot)
        slot += 1

    # Locations: every slot's world holds items, most destined elsewhere. The
    # cross-slot fan-out is the point — it is what makes a release cascade
    # expensive, and what the LocationStore has to survive.
    locations = {}
    for s in sorted(slot_info):
        game = slot_games.get(s)
        if game is None:
            # Spectators and groups own no locations, matching real seeds.
            continue
        loc_pool = per_game_locs[game]
        n = min(args.locations_per_slot, len(loc_pool))
        chosen = rng.sample(loc_pool, n)
        table = {}
        for loc in chosen:
            receiver = rng.choice(player_slots)
            item = rng.choice(per_game_items[slot_games[receiver]])
            # Roughly Archipelago's mix: mostly filler, some progression, few traps.
            roll = rng.random()
            flags = 0b001 if roll < 0.25 else (0b010 if roll < 0.4 else (0b100 if roll < 0.45 else 0))
            table[loc] = (item, receiver, flags)
        locations[s] = table

    # Spheres: a plausible progression shape, used for hint ordering.
    spheres = []
    for depth in range(30):
        sphere = {}
        for s in player_slots[:: max(1, len(player_slots) // 200)]:
            locs = list(locations.get(s, {}))
            if locs:
                sphere[s] = set(rng.sample(locs, min(3, len(locs))))
        if sphere:
            spheres.append(sphere)

    precollected_items = {
        s: [rng.choice(per_game_items[slot_games[s]]) for _ in range(rng.randint(0, 3))]
        for s in player_slots
    }

    precollected_hints = {}
    for s in rng.sample(player_slots, min(50, len(player_slots))):
        finder = rng.choice(player_slots)
        loc_table = locations.get(finder, {})
        if not loc_table:
            continue
        loc = rng.choice(list(loc_table))
        item, receiver, flags = loc_table[loc]
        precollected_hints[s] = {
            Hint(receiver, finder, loc, item, False, "", flags, HintStatus.HINT_UNSPECIFIED)
        }

    er_hint_data = {
        s: {loc: f"Entrance {i}" for i, loc in enumerate(list(locations.get(s, {}))[:20])}
        for s in player_slots[:50]
    }

    seed_name = hashlib.sha256(f"pahoa-large-{args.slots}-{args.seed}".encode()).hexdigest()[:20]

    multidata = {
        "slot_data": {s: {"synthetic": True, "slot": s} for s in player_slots},
        "slot_info": slot_info,
        "connect_names": connect_names,
        "locations": locations,
        "checks_in_area": {},
        "server_options": {"hint_cost": 10, "location_check_points": 1},
        "er_hint_data": er_hint_data,
        "precollected_items": precollected_items,
        "precollected_hints": precollected_hints,
        "version": (0, 6, 8),
        "tags": ["AP"],
        "minimum_versions": {
            "server": (0, 5, 0),
            "clients": {s: (0, 5, 0) for s in player_slots},
        },
        "seed_name": seed_name,
        "spheres": spheres,
        "datapackage": datapackage,
        "race_mode": 0,
    }

    payload = zlib.compress(pickle.dumps(multidata, protocol=4), 9)
    with open(args.out, "wb") as f:
        f.write(bytes([3]))
        f.write(payload)

    total_locations = sum(len(v) for v in locations.values())
    print(
        f"wrote {args.out}: {len(slot_info)} slots "
        f"({n_players} players, {args.spectators} spectators, {args.groups} groups), "
        f"{total_locations} locations, {len(datapackage)} games, "
        f"{len(payload) + 1} bytes"
    )


if __name__ == "__main__":
    main()
