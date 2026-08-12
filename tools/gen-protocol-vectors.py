#!/usr/bin/env python3
"""Generate reference encodings for every server->client packet type.

pahoa must produce byte-identical JSON to Archipelago, not merely equivalent
JSON: key order, the `"class"` tags, compact separators and non-ASCII
passthrough are all observable, and a client that caches or hashes a payload
would notice. This emits the reference bytes so the Rust codec can be compared
against them without Python in the loop.

The encoder here reimplements `NetUtils.encode` (`NetUtils.py:98-139`) rather
than importing Archipelago:

    _encode = JSONEncoder(ensure_ascii=False, check_circular=False,
                          separators=(',', ':')).encode
    def encode(obj): return _encode(_scan_for_TypedTuples(obj))

where `_scan_for_TypedTuples` rewrites every NamedTuple into its `_asdict()`
plus a trailing `"class"` key.

Usage:
    tools/gen-protocol-vectors.py > crates/pahoa-proto/tests/vectors.txt

Output is one `<case name><TAB><json>` line per packet.
"""

import collections
import json
import sys

_encode = json.JSONEncoder(
    ensure_ascii=False, check_circular=False, separators=(",", ":")
).encode


def tagged(cls_name, **fields):
    """A NamedTuple as the wire sees it: fields in order, then "class"."""
    out = dict(fields)
    out["class"] = cls_name
    return out


def version(major, minor, build):
    return tagged("Version", major=major, minor=minor, build=build)


def network_item(item, location, player, flags=0):
    return tagged(
        "NetworkItem", item=item, location=location, player=player, flags=flags
    )


def network_player(team, slot, alias, name):
    return tagged("NetworkPlayer", team=team, slot=slot, alias=alias, name=name)


def network_slot(name, game, type_, group_members=()):
    return tagged(
        "NetworkSlot",
        name=name,
        game=game,
        type=type_,
        group_members=list(group_members),
    )


def hint(recv, find, loc, item, found, entrance, flags, status):
    return tagged(
        "Hint",
        receiving_player=recv,
        finding_player=find,
        location=loc,
        item=item,
        found=found,
        entrance=entrance,
        item_flags=flags,
        status=status,
    )


# The four `PrintJSON` part builders (`NetUtils.py:359-370`, `:388-390`).
# Key order differs between them — `text` first, part-specific keys next, `type`
# last — and it is observable, so these mirror the dict literals exactly.
# `tools/gen-message-vectors.py` pins the same order against the real functions.
def json_text(text, **kwargs):
    return {"text": str(text), **kwargs}


def json_item(item_id, player=0, item_flags=0, **kwargs):
    return {
        "text": str(item_id),
        "player": player,
        "flags": item_flags,
        "type": "item_id",
        **kwargs,
    }


def json_location(location_id, player=0, **kwargs):
    return {"text": str(location_id), "player": player, "type": "location_id", **kwargs}


def json_hint_status(hint_status, text, **kwargs):
    return {"text": text, "hint_status": hint_status, "type": "hint_status", **kwargs}


def item_send_parts(item, location, sender, receiver, flags):
    """`MultiServer.json_format_send_event`, minus the self-send branch."""
    return [
        json_text(sender, type="player_id"),
        json_text(" sent "),
        json_item(item, receiver, flags),
        json_text(" to "),
        json_text(receiver, type="player_id"),
        json_text(" ("),
        json_location(location, sender),
        json_text(")"),
    ]


def hint_status_parts(text, status):
    return [json_text(text), json_hint_status(status, str(status))]


CASES = collections.OrderedDict()

CASES["room_info"] = {
    "cmd": "RoomInfo",
    "version": version(0, 6, 8),
    "generator_version": version(0, 6, 4),
    "tags": ["AP"],
    "password": True,
    "permissions": {"collect": 0, "release": 7, "remaining": 2},
    "hint_cost": 10,
    "location_check_points": 1,
    "games": ["Archipelago", "Timespinner"],
    "datapackage_checksums": {"Archipelago": "abc123", "Timespinner": "def456"},
    "seed_name": "12345678901234567890",
    "time": 1700000000.5,
}

CASES["connection_refused"] = {
    "cmd": "ConnectionRefused",
    "errors": ["InvalidSlot", "InvalidPassword"],
}

CASES["connected"] = {
    "cmd": "Connected",
    "team": 0,
    "slot": 3,
    "players": [
        network_player(0, 1, "Alice", "Alice"),
        network_player(0, 2, "Bobby", "Bob"),
    ],
    "missing_locations": [10, 20, 30],
    "checked_locations": [1, 2],
    "slot_info": {
        "1": network_slot("Alice", "Timespinner", 1),
        "2": network_slot("Link", "Archipelago", 2, [1, 3]),
    },
    "hint_points": 42,
    "slot_data": {"nested": {"a": [1, 2, 3]}, "flag": True},
}

CASES["connected_no_slot_data"] = {
    "cmd": "Connected",
    "team": 0,
    "slot": 1,
    "players": [],
    "missing_locations": [],
    "checked_locations": [],
    "slot_info": {},
    "hint_points": 0,
}

CASES["received_items"] = {
    "cmd": "ReceivedItems",
    "index": 0,
    "items": [network_item(1, -2, 0), network_item(77, 1234, 2, 0b101)],
}

CASES["location_info"] = {
    "cmd": "LocationInfo",
    "locations": [network_item(5, 100, 2, 1)],
}

CASES["room_update_partial"] = {"cmd": "RoomUpdate", "hint_points": 12}

CASES["room_update_checked"] = {
    "cmd": "RoomUpdate",
    "checked_locations": [7, 8, 9],
    "hint_points": 3,
}

CASES["print_json_chat"] = {
    "cmd": "PrintJSON",
    "data": [{"text": "hello"}],
    "type": "Chat",
    "slot": 2,
    "message": "hello",
}

CASES["print_json_item_send"] = {
    "cmd": "PrintJSON",
    "data": item_send_parts(item=77, location=1234, sender=1, receiver=2, flags=1),
    "type": "ItemSend",
    "receiving": 2,
    "item": network_item(77, 1234, 1, 1),
}

CASES["print_json_unicode"] = {
    "cmd": "PrintJSON",
    "data": [{"text": "héllo ✓ 日本語"}],
    "type": "Chat",
}

CASES["print_json_hint"] = {
    "cmd": "PrintJSON",
    "data": hint_status_parts("hint", 30),
    "type": "Hint",
    "receiving": 1,
    "item": network_item(5, 100, 2, 1),
    "found": False,
}

CASES["data_package"] = {
    "cmd": "DataPackage",
    "data": {
        "games": {
            "Archipelago": {
                "item_name_to_id": {"Nothing": 0},
                "location_name_to_id": {"Cheat Console": -1},
                "checksum": "aaa",
            },
            "Timespinner": {
                "item_name_to_id": {"Blade": 1, "Orb": 2},
                "location_name_to_id": {"Chest": 10},
                "checksum": "bbb",
            },
        }
    },
}

CASES["invalid_packet_cmd"] = {
    "cmd": "InvalidPacket",
    "type": "cmd",
    "original_cmd": None,
    "text": "Unknown command Nonsense",
}

CASES["invalid_packet_args"] = {
    "cmd": "InvalidPacket",
    "type": "arguments",
    "original_cmd": "LocationChecks",
    "text": "locations must be a list of integers",
}

# Echo replies: the client's own object with cmd rewritten in place and fields
# appended. Key order therefore follows the *request*, including unknown keys.
CASES["retrieved"] = collections.OrderedDict(
    [("cmd", "Retrieved"), ("keys", {"a": 1, "b": None}), ("client_tag", 7)]
)

CASES["set_reply"] = collections.OrderedDict(
    [
        ("cmd", "SetReply"),
        ("key", "counter"),
        ("want_reply", True),
        ("original_value", 1),
        ("value", 2),
        ("slot", 3),
    ]
)

CASES["bounced"] = collections.OrderedDict(
    [
        ("cmd", "Bounced"),
        ("tags", ["DeathLink"]),
        ("data", {"time": 1700000000.5, "cause": "fell", "source": "Alice"}),
    ]
)

CASES["hint_value"] = {
    "cmd": "PrintJSON",
    "data": [{"text": "x"}],
    "type": "Hint",
    "extra_hint": hint(1, 2, 3, 4, False, "", 1, 30),
}


def main():
    print("# generated by tools/gen-protocol-vectors.py", file=sys.stdout)
    print("# <case>\\t<expected json>", file=sys.stdout)
    for name, packet in CASES.items():
        sys.stdout.write(f"{name}\t{_encode(packet)}\n")


if __name__ == "__main__":
    main()
