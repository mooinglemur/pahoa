#!/usr/bin/env python3
"""Export Archipelago's static server data to JSON, for pahoa to load offline.

This is the *only* thing pahoa needs from the Archipelago world system, and it
runs at release time — never in production. It is a direct port of
`WebHostLib/customserver.py:get_static_server_data()`, which is Archipelago's
own proof that a room server needs no world code: `run_server_process` asserts
`"worlds" not in sys.modules` and receives this dict instead.

Four maps per game:
    item_name_to_id, location_name_to_id, checksum   -- also in multidata
    item_name_groups, location_name_groups           -- also in multidata
    hint_blacklist                                   -- NOWHERE ELSE

That last one is why this script exists at all. A freshly generated
`.archipelago` embeds full packages for every game in the seed, so names and
ids are usually available without a snapshot — but `hint_blacklist` is never
serialised into multidata by anything, so without this export `!hint` silently
stops refusing non-hintable names.

Usage:
    export-datapackage.py --archipelago ~/src/Archipelago > datapackage.json

Importing `worlds` executes every installed apworld, which is slow and memory
hungry. That is precisely the cost this export exists to pay once, offline.
"""

import argparse
import json
import sys


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--archipelago",
        required=True,
        help="path to an Archipelago checkout (the directory containing worlds/)",
    )
    ap.add_argument(
        "--indent",
        type=int,
        default=None,
        help="pretty-print with this indent (default: compact)",
    )
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)

    try:
        import worlds
        from Utils import __version__ as ap_version
    except ImportError as e:
        raise SystemExit(
            f"could not import Archipelago from {args.archipelago!r}: {e}\n"
            "Point --archipelago at a checkout and install its requirements."
        )

    registry = worlds.AutoWorldRegister.world_types
    packages = worlds.network_data_package["games"]

    games = {}
    for name, package in packages.items():
        world = registry.get(name)
        entry = {
            "item_name_to_id": package.get("item_name_to_id", {}),
            "location_name_to_id": package.get("location_name_to_id", {}),
            # Sorted so the export is byte-stable across runs and diffs cleanly
            # in review; Archipelago already sorts group members internally.
            "item_name_groups": {
                k: sorted(v) for k, v in sorted(package.get("item_name_groups", {}).items())
            },
            "location_name_groups": {
                k: sorted(v)
                for k, v in sorted(package.get("location_name_groups", {}).items())
            },
        }
        if "checksum" in package:
            entry["checksum"] = package["checksum"]
        # The Archipelago pseudo-game has no world class behind it.
        if world is not None:
            entry["hint_blacklist"] = sorted(world.hint_blacklist)
        games[name] = entry

    out = {
        "archipelago_version": ap_version,
        "games": dict(sorted(games.items())),
    }
    json.dump(out, sys.stdout, indent=args.indent, sort_keys=False, ensure_ascii=False)
    sys.stdout.write("\n")

    print(
        f"exported {len(games)} games from Archipelago {ap_version}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
