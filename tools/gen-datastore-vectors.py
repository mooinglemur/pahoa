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
# behavior: booleans (which are ints), zero (division), negatives (floored
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
    # the vectors record that behavior explicitly rather than leaving it to be
    # discovered by a client.
    "%s",
    "%d items",
    "100%%",
    # Wider than i64 and wider than u64, with both signs. A world storing its
    # location checks as a 71-bit bitfield is what made these necessary: pahoa
    # used to fold them into floats while parsing, so `or` on one was a
    # TypeError and a dropped connection. 2**71 is the reported width; the rest
    # surround it so the sign and the i64/u64 boundaries are all covered.
    2**71,
    2**71 + 1,
    -(2**71),
    2**63,
    2**64,
    -(2**63) - 1,
    [],
    [1],
    [1, 2],
    [1, "a"],
    [[1]],
    {},
    {"a": 1},
    {"b": 2},
]


# pahoa refuses an integer wider than this rather than build it, because these
# operations run on the one task that owns all room state. Keep in step with
# `ops::MAX_INT_BITS`; a result past it is recorded as a known divergence rather
# than as a value pahoa is expected to produce.
MAX_INT_BITS = 65536


def int_operand(value):
    """The integer `value` is for arithmetic, or None. Booleans count."""
    return value if isinstance(value, (int, bool)) else None


def refuses_to_compute(op, current, arg):
    """Whether evaluating this in CPython would be the denial of service.

    Not a nicety: with wide integers in the operand list, `pow(2**71, 2**71)`
    is in the matrix, and CPython will happily try to build a number with
    3 * 10**21 bits. It does not fail — it takes the machine down, which is the
    whole reason `ops::MAX_INT_BITS` exists on pahoa's side.

    The projected width is the same arithmetic pahoa does before allocating, so
    this refuses exactly the cases pahoa refuses, and they are recorded as
    divergences rather than as expected values.
    """
    a, b = int_operand(current), int_operand(arg)
    if a is None or b is None:
        return False
    if op == "pow":
        # 0, 1 and -1 stay themselves however large the exponent.
        if b < 0 or abs(a) <= 1:
            return False
        return a.bit_length() * b > MAX_INT_BITS
    if op == "left_shift":
        return b >= 0 and a.bit_length() + b > MAX_INT_BITS
    if op == "mul":
        return a.bit_length() + b.bit_length() > MAX_INT_BITS
    return False


def jsonable(value):
    """Whether a Python result survives a JSON round trip unchanged.

    Integers of any width are fine now — pahoa keeps their digits — up to the
    width bound above. What is still filtered out is non-finite floats, which
    Python emits as invalid JSON.
    """
    if isinstance(value, bool):
        return True
    if isinstance(value, int):
        return value.bit_length() <= MAX_INT_BITS
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

            if refuses_to_compute(op, current, arg):
                record["skip"] = "result is wider than pahoa will build"
                print(json.dumps(record))
                emitted += 1
                continue

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
