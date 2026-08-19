#!/usr/bin/env python3
"""Reference implementation of `pahoa inspect`, for differential testing.

Produces byte-identical output to the Rust command over the same multidata.
That comparison is M1's exit gate: it exercises slot typing, the location
table, hints, versions and the data-package merge at once, against an
independent implementation.

Like `dump-pickle.py` this deliberately does not import Archipelago — it
reimplements the pieces of the merge policy it needs, so a bug copied from
Archipelago's source would not be copied into both sides at once.

Usage:
    inspect-multidata.py <file.archipelago>
"""

import argparse
import io
import json
import pickle
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

MIN_CLIENT_VERSION = (0, 5, 0)
LEGACY_GENERATOR_CUTOFF = (0, 6, 2)
LEGACY_MIN_CLIENT_VERSION = (0, 1, 6)

SLOT_KIND = {0: "spectator", 1: "player", 2: "group"}

# Mirrors crates/pahoa-multidata/src/hint_blacklist.rs, which mirrors
# World.hint_blacklist across the Archipelago tree. Duplicated deliberately:
# this file exists to be an *independent* check on the Rust loader, so importing
# the same table would make the differential agree with itself for free.
# export-datapackage.py regenerates both from a checkout.
HINT_BLACKLIST = {
    "A Link to the Past": ["Triforce"],
    "Castlevania - Circle of the Moon": ["Battle Arena: End reward"],
}


class Instance:
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


def load(path):
    with open(path, "rb") as f:
        data = f.read()
    fmt = data[0]
    if fmt > 3:
        raise SystemExit(f"unsupported multidata format version {fmt}")
    return StubUnpickler(io.BytesIO(zlib.decompress(data[1:]))).load()


def ver(t):
    return "%d.%d.%d" % tuple(t)


def is_stub(pkg):
    return not pkg.get("item_name_to_id") and not pkg.get("location_name_to_id")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("multidata")
    args = ap.parse_args()

    md = load(args.multidata)


    slot_info = md["slot_info"]
    generator_version = tuple(md["version"])
    floor = (
        LEGACY_MIN_CLIENT_VERSION
        if generator_version < LEGACY_GENERATOR_CUTOFF
        else MIN_CLIENT_VERSION
    )
    client_floors = {
        int(s): max(tuple(v), floor)
        for s, v in md["minimum_versions"].get("clients", {}).items()
    }

    print("seed_name: %s" % md["seed_name"])
    print("generator_version: %s" % ver(generator_version))
    print("minimum_server_version: %s" % ver(md["minimum_versions"]["server"]))
    print("race_mode: %s" % ("true" if md.get("race_mode", 0) else "false"))

    kinds = {"player": 0, "spectator": 0, "group": 0}
    for info in slot_info.values():
        kinds[SLOT_KIND[info.args[2].args[0]]] += 1
    print("slots: %d" % len(slot_info))
    print("slots_player: %d" % kinds["player"])
    print("slots_spectator: %d" % kinds["spectator"])
    print("slots_group: %d" % kinds["group"])

    locations = md["locations"]
    print("connect_names: %d" % len(md["connect_names"]))
    print("locations_total: %d" % sum(len(v) for v in locations.values()))
    print("locations_max_slot: %d" % (max(locations) if locations else 0))
    print("spheres: %d" % len(md.get("spheres", [])))
    print(
        "precollected_items: %d"
        % sum(len(v) for v in md.get("precollected_items", {}).values())
    )
    print(
        "precollected_hints: %d"
        % sum(len(v) for v in md.get("precollected_hints", {}).values())
    )
    print(
        "er_hint_data: %d" % sum(len(v) for v in md.get("er_hint_data", {}).values())
    )
    print("slot_data_slots: %d" % len(md.get("slot_data", {})))
    print("server_options: %s" % ("true" if "server_options" in md else "false"))

    # Merge policy, mirrored from pahoa_multidata::DataPackage::merge.
    embedded = md.get("datapackage", {})
    needed = {info.args[1] for info in slot_info.values()} | {"Archipelago"}

    merged = {}
    from_multidata = unresolved = 0
    for game in needed:
        emb = embedded.get(game)
        if emb is not None and is_stub(emb):
            emb = None
        if emb is not None:
            pkg = dict(emb)
            from_multidata += 1
        else:
            pkg = {}
            unresolved += 1
        # Compiled into pahoa rather than serialized anywhere; mirrored here so
        # the two implementations still produce identical text.
        pkg["hint_blacklist"] = HINT_BLACKLIST.get(game, [])
        merged[game] = pkg

    print("games: %d" % len(merged))
    print("datapackage_embedded: %d" % len(embedded))
    print("datapackage_from_multidata: %d" % from_multidata)
    print("datapackage_unresolved: %d" % unresolved)

    for game in sorted(merged):
        pkg = merged[game]
        print(
            "game %s: items=%d locations=%d item_groups=%d location_groups=%d blacklist=%d checksum=%s"
            % (
                game,
                len(pkg.get("item_name_to_id", {})),
                len(pkg.get("location_name_to_id", {})),
                len(pkg.get("item_name_groups", {})),
                len(pkg.get("location_name_groups", {})),
                len(set(pkg.get("hint_blacklist", []))),
                pkg.get("checksum", "-"),
            )
        )

    for slot in sorted(slot_info):
        info = slot_info[slot]
        name, game, slot_type, members = info.args
        print(
            "slot %d: kind=%s name=%s game=%s locations=%d min_client=%s members=[%s]"
            % (
                slot,
                SLOT_KIND[slot_type.args[0]],
                name,
                game,
                len(locations.get(slot, {})),
                ver(client_floors.get(slot, floor)),
                ",".join(str(m) for m in members),
            )
        )


if __name__ == "__main__":
    main()
