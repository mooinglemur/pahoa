#!/usr/bin/env python3
"""Regenerate pahoa's built-in hint blacklist from an Archipelago checkout.

`World.hint_blacklist` (`worlds/AutoWorld.py:312`) is "any names that should not
be hintable". The reference server reads it out of its installed worlds at
`MultiServer.py:343-344` and `!hint` refuses a match against it. **It is never
serialized into multidata by anything** — it is Python class data — so a
standalone server has to carry its own copy.

pahoa compiles that copy in, rather than loading a JSON snapshot at startup:
an external file can be missing, stale, or forgotten by whoever deploys the
room, and a table in the binary can be none of those. This script is what keeps
the table honest, so the values stay *derived* from Archipelago rather than
hand-copied and slowly wrong.

Everything else a room needs — item and location names, ids, name groups,
checksums — is embedded in the seed itself, so nothing else needs exporting.

Usage:
    export-datapackage.py --archipelago ~/src/Archipelago            # show a diff
    export-datapackage.py --archipelago ~/src/Archipelago --write    # apply it

Importing `worlds` executes every installed apworld, which is slow and memory
hungry. That is precisely the cost this exists to pay once, offline, rather than
in a room.

**It needs a checkout whose apworlds can all import**, which is a stronger
requirement than it sounds: a bare Archipelago venv is missing several worlds'
own dependencies, and a world that fails to import is simply absent from the
registry rather than reported. Since this script's output *deletes* entries, an
incomplete registry would quietly stop `!hint` refusing a name. It therefore
refuses to run at all when anything failed to load.

If that cannot be arranged, the fallback is to read the source directly —
`grep -rn hint_blacklist worlds/` finds every world that sets one, since it is
always a class-level assignment — and follow the constants by hand. That is how
the table's current entries were established, and both were cross-checked
against `MultiServer.py`'s use of them.
"""

import argparse
import pathlib
import re
import sys

TABLE = pathlib.Path(__file__).resolve().parent.parent / (
    "crates/pahoa-multidata/src/hint_blacklist.rs"
)
MIRROR = pathlib.Path(__file__).resolve().parent / "inspect-multidata.py"

BEGIN = "pub const HINT_BLACKLIST: &[(&str, &[&str])] = &["
END = "];"


def rust_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def collect(archipelago):
    sys.path.insert(0, archipelago)
    try:
        import worlds
        from Utils import __version__ as ap_version
    except ImportError as e:
        raise SystemExit(
            f"could not import Archipelago from {archipelago!r}: {e}\n"
            "Point --archipelago at a checkout and install its requirements."
        )

    # A world that failed to import is simply absent from the registry, so a
    # checkout with unmet dependencies produces a *silently short* table — and
    # this script's output deletes entries, so short means "stop refusing to
    # hint Triforce". Refuse rather than guess.
    failed = getattr(worlds, "failed_world_loads", {})
    if failed:
        names = ", ".join(sorted(failed))
        raise SystemExit(
            f"{len(failed)} world(s) failed to import, so the registry is incomplete "
            f"and this table would silently lose entries:\n  {names}\n"
            "Install the checkout's requirements (including every apworld's own) "
            "and run again."
        )

    found = {}
    for name, world in worlds.AutoWorldRegister.world_types.items():
        blacklist = sorted(world.hint_blacklist)
        if blacklist:
            found[name] = blacklist
    return dict(sorted(found.items())), ap_version


def render(found, ap_version):
    lines = [BEGIN]
    for game, names in found.items():
        entries = ", ".join(rust_str(n) for n in names)
        lines.append(f"    ({rust_str(game)}, &[{entries}]),")
    lines.append(END)
    return "\n".join(lines), ap_version


def current():
    """The table as it stands, parsed back out of the Rust source."""
    text = TABLE.read_text()
    body = text[text.index(BEGIN) + len(BEGIN) : text.index(END, text.index(BEGIN))]
    out = {}
    for game, entries in re.findall(r'\(\s*"((?:[^"\\]|\\.)*)"\s*,\s*&\[(.*?)\]\s*\)', body, re.S):
        names = re.findall(r'"((?:[^"\\]|\\.)*)"', entries)
        out[game.replace('\\"', '"')] = [n.replace('\\"', '"') for n in names]
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True,
                    help="path to an Archipelago checkout (the directory containing worlds/)")
    ap.add_argument("--write", action="store_true",
                    help="rewrite the table in place instead of only reporting")
    args = ap.parse_args()

    found, ap_version = collect(args.archipelago)
    have = current()

    if found == have:
        print(f"up to date against Archipelago {ap_version}: "
              f"{len(found)} game(s) with a hint blacklist", file=sys.stderr)
        return 0

    # A change here is a behavior change in `!hint`, so it is reported rather
    # than applied quietly even with --write.
    for game in sorted(set(have) | set(found)):
        before, after = have.get(game), found.get(game)
        if before != after:
            print(f"  {game}: {before!r} -> {after!r}", file=sys.stderr)

    if not args.write:
        print("\nrun again with --write to apply, then review the diff and update\n"
              f"the mirrored table in {MIRROR.name}", file=sys.stderr)
        return 1

    text = TABLE.read_text()
    start = text.index(BEGIN)
    stop = text.index(END, start) + len(END)
    block, _ = render(found, ap_version)
    TABLE.write_text(text[:start] + block + text[stop:])
    print(f"\nwrote {TABLE}", file=sys.stderr)
    print(f"NOW UPDATE {MIRROR} — it mirrors this table on purpose, so that the\n"
          "inspect differential stays an independent check rather than agreeing\n"
          "with itself.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
