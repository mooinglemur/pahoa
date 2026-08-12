#!/usr/bin/env python3
"""Generate data-storage operation vectors from Archipelago's own table.

The eighteen `Set` operations are Python expressions over client-supplied JSON.
Hand-writing tests for 18 operations across every combination of null, bool,
int, float, string, list and dict is hopeless — so this enumerates them against
the *real* `MultiServer.modify_functions`, records what CPython does (including
which exception it raises), and commits the result for the Rust side to replay.

Importing `MultiServer` rather than reimplementing the table is the point: a
transcription error here would be copied into both sides and prove nothing.

    ~/src/Archipelago/.venv/bin/python tools/gen-datastore-vectors.py \\
        --archipelago ~/src/Archipelago \\
        > crates/pahoa-datastore/tests/vectors.jsonl

One JSON object per line:
    {"op":…, "current":…, "arg":…, "result":…}       CPython produced a value
    {"op":…, "current":…, "arg":…, "error":"TypeError"}   CPython raised
    {"op":…, "current":…, "arg":…, "skip":"reason"}   not representable in JSON
"""

import argparse
import copy
import itertools
import json
import sys

# Deliberately spread across every JSON type, with the edge cases that decide
# behaviour: booleans (which are ints), zero (division), negatives (floored
# modulo), floats that are integral, empty containers, and nested containers
# (which are unhashable and so break `update` on a list).
OPERANDS = [
    None,
    True,
    False,
    0,
    1,
    -1,
    7,
    -7,
    3,
    -3,
    2.5,
    -2.5,
    2.0,
    "",
    "ab",
    "1",
    # `%` on a str is printf-style formatting in Python, not modulo. Included so
    # the vectors record that behaviour explicitly rather than leaving it to be
    # discovered by a client.
    "%s",
    "%d items",
    "100%%",
    [],
    [1],
    [1, 2],
    [1, "a"],
    [[1]],
    {},
    {"a": 1},
    {"b": 2},
]


def jsonable(value):
    """Whether a Python result survives a JSON round trip unchanged.

    Filters out what neither side can represent: integers beyond 64 bits (which
    is a documented pahoa divergence), and non-finite floats (which Python emits
    as invalid JSON).
    """
    if isinstance(value, bool):
        return True
    if isinstance(value, int):
        return -(2**63) <= value < 2**63
    if isinstance(value, float):
        return value == value and value not in (float("inf"), float("-inf"))
    if isinstance(value, str):
        return True
    if value is None:
        return True
    if isinstance(value, list):
        return all(jsonable(v) for v in value)
    if isinstance(value, dict):
        return all(isinstance(k, str) and jsonable(v) for k, v in value.items())
    return False


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]
    import ModuleUpdate

    ModuleUpdate.update_ran = True
    import MultiServer

    functions = MultiServer.modify_functions
    emitted = 0

    for op in sorted(functions):
        f = functions[op]
        for current, arg in itertools.product(OPERANDS, OPERANDS):
            record = {"op": op, "current": current, "arg": arg}

            # The operations mutate their container in place, so each case gets
            # a fresh copy — otherwise earlier cases would contaminate later ones.
            try:
                result = f(copy.deepcopy(current), copy.deepcopy(arg))
            except Exception as e:  # noqa: BLE001 - recording is the point
                record["error"] = type(e).__name__
                print(json.dumps(record))
                emitted += 1
                continue

            if not jsonable(result):
                record["skip"] = "result is not representable in JSON"
            else:
                record["result"] = result
            print(json.dumps(record))
            emitted += 1

    print(f"emitted {emitted} vectors for {len(functions)} operations", file=sys.stderr)


if __name__ == "__main__":
    main()
