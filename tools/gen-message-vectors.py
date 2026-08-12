#!/usr/bin/env python3
"""Generate PrintJSON vectors from Archipelago's own message builders.

Two functions produce nearly every line of chat a room emits, and both are
easy to get subtly wrong by writing out what the message *reads* like instead
of what it *is*:

- `MultiServer.json_format_send_event` — the item feed. It sends bare **ids**
  with `item_id`/`location_id` part types, not resolved names, and it has a
  separate "found their" phrasing when a slot sends to itself.
- `NetUtils.Hint.as_network_message` — the hint line, including the entrance
  clause and the trailing status label.

Encoding goes through `NetUtils.encode`, so the vectors are byte-exact and also
pin the `"class"` tagging of the embedded `NetworkItem`.

    ~/src/Archipelago/.venv/bin/python tools/gen-message-vectors.py \\
        --archipelago ~/src/Archipelago \\
        > crates/pahoa-room/tests/message_vectors.jsonl

One JSON object per line:
    {"kind":"item_send", "item":{…}, "receiving":…, "encoded":"…"}
    {"kind":"hint", "hint":{…}, "encoded":"…"}
"""

import argparse
import json
import sys

# Sender/receiver pairs covering the two phrasings and a few flag combinations.
# Flags are the low three ItemClassification bits: advancement, useful, trap.
ITEM_SENDS = [
    # (item, location, sending player, flags, receiving player)
    (42, 100, 1, 0b000, 2),
    (42, 100, 1, 0b001, 2),
    (42, 100, 1, 0b100, 2),
    (42, 100, 1, 0b111, 2),
    # Self-send: "found their" rather than "sent … to".
    (7, 55, 3, 0b010, 3),
    (7, 55, 3, 0b000, 3),
    # Negative ids are the cheat/start-inventory sentinels.
    (9, -1, 1, 0b000, 1),
    (9, -2, 0, 0b000, 4),
    # Ids well past 2^32, which real worlds use.
    (0xDEADBEEF01, 0xCAFEBABE02, 5, 0b001, 6),
]

ENTRANCES = ["", "Front Door", "Kakariko Well Drop"]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]
    import ModuleUpdate

    ModuleUpdate.update_ran = True

    from NetUtils import Hint, HintStatus, NetworkItem, encode
    from MultiServer import json_format_send_event

    emitted = 0

    for item, location, player, flags, receiving in ITEM_SENDS:
        net_item = NetworkItem(item, location, player, flags)
        msg = json_format_send_event(net_item, receiving)
        print(
            json.dumps(
                {
                    "kind": "item_send",
                    "item": {
                        "item": item,
                        "location": location,
                        "player": player,
                        "flags": flags,
                    },
                    "receiving": receiving,
                    "encoded": encode(msg),
                }
            )
        )
        emitted += 1

    for status in HintStatus:
        for entrance in ENTRANCES:
            for found in (False, True):
                # Local (receiver == finder) and remote placements both matter:
                # the message text is the same but the ids differ, and getting
                # the two players the wrong way round is the classic bug here.
                for receiving, finding in ((2, 1), (1, 1)):
                    hint = Hint(
                        receiving, finding, 100, 42, found, entrance, 0b001, status
                    )
                    print(
                        json.dumps(
                            {
                                "kind": "hint",
                                "hint": {
                                    "receiving_player": receiving,
                                    "finding_player": finding,
                                    "location": 100,
                                    "item": 42,
                                    "found": found,
                                    "entrance": entrance,
                                    "item_flags": 0b001,
                                    "status": int(status),
                                },
                                "encoded": encode(hint.as_network_message()),
                            }
                        )
                    )
                    emitted += 1

    print(f"emitted {emitted} message vectors", file=sys.stderr)


if __name__ == "__main__":
    main()
