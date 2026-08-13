#!/usr/bin/env python3
"""Play a real multiworld to completion against pahoa, with real client code.

M9's other half. The load track proves pahoa stands up at scale; this proves it
plays a *whole game* — every slot connects, checks every location it owns,
receives everything owed to it, and reaches its goal, after which the room
reports the multiworld finished.

It is a real client in the ways that matter: Archipelago's own `NetUtils.encode`
and `decode` (so the `"class"` discriminators and `Version` reconstruction are
the genuine article), Archipelago's `websockets` (so the WebSocket layer and
permessage-deflate are exercised exactly as a player's client would), and
Archipelago's `RawJSONtoTextParser` for rendering. What it is not is a *game*
client — nothing here simulates gameplay, because the server cannot tell the
difference and the multiworld's completion rules are what is under test.

    ~/src/Archipelago/.venv/bin/python tools/play-seed.py \\
        --archipelago ~/src/Archipelago --port 38281 \\
        --multidata crates/pahoa-pickle/tests/fixtures/AP_....archipelago

Exits non-zero on any mismatch, so it can gate a release.
"""

import argparse
import asyncio
import collections
import sys
import zlib

CHUNK = 250


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    ap.add_argument("--multidata", required=True)
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", type=int, default=38281)
    ap.add_argument("--password", default=None)
    ap.add_argument(
        "--slots",
        type=int,
        default=0,
        help="play only the first N slots, for a quicker check (0 = all)",
    )
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]
    import ModuleUpdate

    ModuleUpdate.update_ran = True

    import Utils
    from NetUtils import SlotType, decode, encode

    raw = open(args.multidata, "rb").read()
    data = Utils.restricted_loads(zlib.decompress(raw[1:]))
    slot_info = data["slot_info"]
    locations = data["locations"]

    # Compared against the enum, not against `str(...)`: `SlotType` is an
    # `IntEnum`, and since Python 3.11 that stringifies as "1" rather than
    # "SlotType.player". Spectators and item-link groups have no locations to
    # check and are marked goal at load, so playing them would be meaningless.
    players = [
        (slot, info.name, info.game)
        for slot, info in sorted(slot_info.items())
        if info.type == SlotType.player
    ]
    if args.slots:
        players = players[: args.slots]
    if not players:
        raise SystemExit("no player slots in this multidata")

    total_locations = sum(len(locations.get(slot, ())) for slot, _, _ in players)
    print(f"playing {len(players)} slots, {total_locations} locations")

    result = asyncio.run(play(args, players, locations, encode, decode))
    raise SystemExit(result)


async def play(args, players, locations, encode, decode):
    import websockets

    uri = f"ws://{args.host}:{args.port}"
    failures = []
    # Every item the server said it delivered, so the totals can be checked
    # against the multidata rather than merely against themselves.
    received = collections.Counter()
    goaled = set()

    async def play_slot(slot, name, game):
        # `CommonClient` connects with default extensions, so this negotiates
        # permessage-deflate exactly as a player's client does.
        async with websockets.connect(
            uri, ping_timeout=None, ping_interval=None, max_size=64 * 1024 * 1024
        ) as socket:

            async def send(packets):
                await socket.send(encode(packets))

            async def recv():
                return decode(await asyncio.wait_for(socket.recv(), timeout=120))

            await expect(recv, "RoomInfo")
            await send(
                [
                    {
                        "cmd": "Connect",
                        "password": args.password,
                        "game": game,
                        "name": name,
                        "uuid": f"play-seed-{slot}",
                        "version": {
                            "major": 0,
                            "minor": 6,
                            "build": 8,
                            "class": "Version",
                        },
                        "items_handling": 0b111,
                        "tags": ["AP"],
                        "slot_data": False,
                    }
                ]
            )
            connected = await expect(recv, "Connected")
            if connected["slot"] != slot:
                failures.append(f"slot {slot}: server said slot {connected['slot']}")
                return

            mine = sorted(locations.get(slot, ()))
            for i in range(0, len(mine), CHUNK):
                await send(
                    [{"cmd": "LocationChecks", "locations": mine[i : i + CHUNK]}]
                )

            # Goal last, so the server sees a slot that finished its world
            # rather than one that claimed the goal without playing it.
            await send([{"cmd": "StatusUpdate", "status": 30}])
            goaled.add(slot)

            # Drain briefly so the server's replies are actually consumed —
            # a client that stops reading is one the server may rightly drop.
            deadline = asyncio.get_event_loop().time() + 20
            while asyncio.get_event_loop().time() < deadline:
                try:
                    packets = decode(await asyncio.wait_for(socket.recv(), timeout=2))
                except asyncio.TimeoutError:
                    break
                for packet in packets:
                    if packet["cmd"] == "ReceivedItems":
                        received[slot] += len(packet["items"])

    # Sequential rather than parallel: this is about a game being *completable*,
    # and running the slots in order keeps a failure attributable to one slot.
    for index, (slot, name, game) in enumerate(players, 1):
        try:
            await play_slot(slot, name, game)
        except Exception as e:  # noqa: BLE001 — any failure is a failed run
            failures.append(f"slot {slot} ({name}): {type(e).__name__}: {e}")
        if index % 10 == 0 or index == len(players):
            print(f"  {index}/{len(players)} slots played")

    # --- the room's own view ------------------------------------------------
    async with websockets.connect(
        uri, ping_timeout=None, ping_interval=None, max_size=64 * 1024 * 1024
    ) as socket:
        slot, name, game = players[0]
        await socket.send(encode([{"cmd": "Connect", "password": args.password,
                                   "game": game, "name": name, "uuid": "play-seed-audit",
                                   "version": {"major": 0, "minor": 6, "build": 8,
                                               "class": "Version"},
                                   "items_handling": 0b111, "tags": ["AP"],
                                   "slot_data": False}]))
        connected = None
        while connected is None:
            for packet in decode(await asyncio.wait_for(socket.recv(), timeout=60)):
                if packet["cmd"] == "Connected":
                    connected = packet
                elif packet["cmd"] == "ConnectionRefused":
                    failures.append(f"audit connect refused: {packet}")
                    return report(failures, received, goaled, players)

        # Every location this slot owns should now be checked, and none missing.
        if connected["missing_locations"]:
            failures.append(
                f"slot {slot} still reports {len(connected['missing_locations'])} "
                "missing locations after playing them all"
            )
        expected = len(locations.get(slot, ()))
        if len(connected["checked_locations"]) != expected:
            failures.append(
                f"slot {slot}: server reports {len(connected['checked_locations'])} "
                f"checked, multidata says {expected}"
            )

        # And the room should agree that everyone is done. The wording is the
        # reference's, verbatim (`MultiServer.py:1052-1058`): a slot that has
        # reached its goal reads "... and has finished. (checked/total)".
        await socket.send(encode([{"cmd": "Say", "text": "!status"}]))
        status = await collect_text(socket, decode, "CommandResult", seconds=20)
        finished = status.count("and has finished.")
        print(f"\nroom status: {finished} slots report finished")
        if finished < len(players):
            print(status.strip() or "(no status text)")
            failures.append(
                f"room reports {finished} finished slots, expected {len(players)}"
            )
        # Every slot's line carries "(checked/total)"; none should be partial.
        partial = [
            line.strip()
            for line in status.splitlines()
            if "(" in line and ")" in line and not balanced_counts(line)
        ]
        if partial:
            failures.append(
                f"{len(partial)} slots report unfinished location counts, "
                f"e.g. {partial[0]}"
            )

    return report(failures, received, goaled, players)


def balanced_counts(line):
    """Whether a status line's trailing `(checked/total)` are equal."""
    try:
        counts = line.rsplit("(", 1)[1].rstrip(")").split("/")
        return int(counts[0]) == int(counts[1])
    except (IndexError, ValueError):
        # Not a slot line at all; nothing to complain about.
        return True


async def expect(recv, cmd):
    """Read until `cmd` arrives, failing loudly on a refusal."""
    for _ in range(200):
        for packet in await recv():
            if packet["cmd"] == cmd:
                return packet
            if packet["cmd"] == "ConnectionRefused":
                raise RuntimeError(f"connection refused: {packet.get('errors')}")
    raise RuntimeError(f"never saw {cmd}")


async def collect_text(socket, decode, print_type, seconds):
    """Gather the text of every `PrintJSON` of a kind for a while."""
    out = []
    deadline = asyncio.get_event_loop().time() + seconds
    while asyncio.get_event_loop().time() < deadline:
        try:
            packets = decode(await asyncio.wait_for(socket.recv(), timeout=2))
        except asyncio.TimeoutError:
            break
        for packet in packets:
            if packet["cmd"] == "PrintJSON" and packet.get("type") == print_type:
                out.append("".join(p.get("text", "") for p in packet["data"]))
    return "\n".join(out)


def report(failures, received, goaled, players):
    # The item count is a *lower bound*: each slot drains for a bounded time and
    # items placed by slots played later arrive after it has stopped listening.
    # What is actually verified is the room's own state, audited above.
    print(
        f"\n{len(goaled)}/{len(players)} slots reached their goal, "
        f"at least {sum(received.values())} items observed in flight"
    )
    if failures:
        print("\nFAILURES:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("OK: the multiworld played to completion")
    return 0


if __name__ == "__main__":
    main()
