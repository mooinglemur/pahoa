#!/usr/bin/env python3
"""Generate name-matching vectors from Archipelago's own fuzzy matcher.

`!hint` and `!getitem` route player typos through `Utils.get_intended_text`,
whose thresholds decide whether a near-miss becomes a hint, a "did you mean…",
or a refusal. Getting one of those boundaries wrong spends a player's hint
points on the wrong item, so the scoring is pinned against the real function
rather than against a reading of it.

Uses Archipelago's `Utils` directly, which in turn uses jellyfish — so this also
confirms `strsim::damerau_levenshtein` is the same algorithm jellyfish provides.

    ~/src/Archipelago/.venv/bin/python tools/gen-fuzzy-vectors.py \\
        --archipelago ~/src/Archipelago \\
        > crates/pahoa-room/tests/fuzzy_vectors.jsonl

One JSON object per line:
    {"input":…, "candidates":[…], "scores":[…], "picked":…, "accepted":bool}
"""

import argparse
import json
import sys

# Item and location names shaped like the ones players actually mistype:
# multi-word, shared prefixes, differing case, punctuation, and non-ASCII.
CANDIDATE_SETS = [
    ["Sword", "Shield", "Bow"],
    ["Sword", "Sworn", "Swore"],
    ["Progressive Sword", "Progressive Shield", "Progressive Bow"],
    ["Big Key", "Small Key", "Boss Key"],
    ["Chest 1", "Chest 2", "Chest 12"],
    ["Hookshot", "Longshot"],
    ["Blue Potion", "Red Potion", "Green Potion"],
    ["Piece of Heart", "Heart Container"],
    ["Café Key", "Cafe Key"],
    ["A"],
    ["Bombchu (10)", "Bombchu (5)", "Bombchus"],
]

INPUTS = [
    "Sword",
    "sword",
    "SWORD",
    "Swrod",
    "Swor",
    "sworrd",
    "Progressive Sward",
    "progressive sword",
    "Big Ky",
    "key",
    "Chest 1",
    "Chest",
    "Hookshot",
    "Hoookshot",
    "Longshot",
    "Blue Potio",
    "Potion",
    "Heart",
    "Piece of Hart",
    "Cafe Key",
    "Café Key",
    "A",
    "zzzzzzzzzz",
    "",
    "Bombchu",
    "Bombchu (1)",
]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]
    import ModuleUpdate

    ModuleUpdate.update_ran = True
    from Utils import get_fuzzy_results, get_intended_text

    emitted = 0
    for candidates in CANDIDATE_SETS:
        for text in INPUTS:
            # A list, not a set: Python's sort is stable, so tie order follows
            # the collection's order, and a set would make that unreproducible.
            scores = [score for _, score in get_fuzzy_results(text, candidates)]
            names = [name for name, _ in get_fuzzy_results(text, candidates)]
            picked, accepted, reason = get_intended_text(text, candidates)
            print(
                json.dumps(
                    {
                        "input": text,
                        "candidates": candidates,
                        "ranked": names,
                        "scores": scores,
                        "picked": picked,
                        "accepted": accepted,
                        "reason": reason,
                    }
                )
            )
            emitted += 1

    print(f"emitted {emitted} fuzzy vectors", file=sys.stderr)


if __name__ == "__main__":
    main()
