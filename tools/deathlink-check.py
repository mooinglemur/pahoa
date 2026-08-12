#!/usr/bin/env python3
"""Round-trip DeathLink between two real Archipelago clients through pahoa.

M5's exit gate. DeathLink is the most-used thing built on `Bounce`, and it is
the reason data storage sits on the critical path for "finish a real seed"
rather than being a later nicety. Both ends here use Archipelago's own
`NetUtils` codec and `websockets`, so this exercises the wire format, the tag
filter and the echo semantics together.

    ~/src/Archipelago/.venv/bin/python tools/deathlink-check.py \\
        --archipelago ~/src/Archipelago --port 38281 \\
        --slot-a "PlayerOne" --game-a "Some Game" \\
        --slot-b "PlayerTwo" --game-b "Other Game"

Exits non-zero on failure.
"""

import argparse
import asyncio
import sys
import time


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", type=int, default=38281)
    ap.add_argument("--slot-a", required=True)
    ap.add_argument("--game-a", required=True)
    ap.add_argument("--slot-b", required=True)
    ap.add_argument("--game-b", required=True)
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]
    import ModuleUpdate

    ModuleUpdate.update_ran = True

    import websockets
    import Utils
    from NetUtils import decode, encode

    async def connect(slot, game, tags):
        socket = await websockets.connect(
            f"ws://{args.host}:{args.port}",
            ping_timeout=None,
            ping_interval=None,
            max_size=16 * 1024 * 1024,
        )

        async def recv():
            return decode(await asyncio.wait_for(socket.recv(), timeout=10))

        async def send(packets):
            await socket.send(encode(packets))

        # RoomInfo, then authenticate.
        await recv()
        await send(
            [
                {
                    "cmd": "Connect",
                    "password": None,
                    "name": slot,
                    "version": Utils.version_tuple,
                    "tags": tags,
                    "items_handling": 0b001,
                    "uuid": Utils.get_unique_identifier(),
                    "game": game,
                    "slot_data": False,
                }
            ]
        )
        for _ in range(10):
            for packet in await recv():
                if packet["cmd"] == "ConnectionRefused":
                    raise SystemExit(f"{slot} refused: {packet['errors']}")
                if packet["cmd"] == "Connected":
                    return socket, send, recv, packet
        raise SystemExit(f"{slot} never connected")

    async def run():
        # Both carry the DeathLink tag, which is how the server decides who a
        # bounce reaches.
        _, send_a, recv_a, conn_a = await connect(
            args.slot_a, args.game_a, ["AP", "DeathLink"]
        )
        _, send_b, recv_b, conn_b = await connect(
            args.slot_b, args.game_b, ["AP", "DeathLink"]
        )
        print(f"connected: slot {conn_a['slot']} and slot {conn_b['slot']}")

        death = {
            "time": time.time(),
            "cause": f"{args.slot_a} fell down a hole",
            "source": args.slot_a,
        }
        await send_a([{"cmd": "Bounce", "tags": ["DeathLink"], "data": death}])

        async def wait_bounced(recv, who):
            for _ in range(20):
                for packet in await recv():
                    if packet["cmd"] == "Bounced":
                        return packet
            raise SystemExit(f"{who} never received the Bounced packet")

        # Archipelago forwards to everyone matching the tag, sender included.
        bounced_b = await wait_bounced(recv_b, args.slot_b)
        bounced_a = await wait_bounced(recv_a, args.slot_a)

        for name, packet in ((args.slot_b, bounced_b), (args.slot_a, bounced_a)):
            assert packet["data"]["cause"] == death["cause"], (
                f"{name} got a corrupted payload: {packet['data']}"
            )
            assert packet["data"]["source"] == args.slot_a, packet["data"]
            assert packet["tags"] == ["DeathLink"], packet
            print(f"  {name} received: {packet['data']['cause']}")

        # Data storage, exercised the way a tracker would.
        await send_a(
            [
                {
                    "cmd": "Set",
                    "key": "deaths",
                    "default": 0,
                    "want_reply": True,
                    "operations": [{"operation": "add", "value": 1}],
                }
            ]
        )
        for _ in range(20):
            done = False
            for packet in await recv_a():
                if packet["cmd"] == "SetReply":
                    assert packet["value"] == 1, packet
                    assert packet["original_value"] == 0, packet
                    print(f"  SetReply: {packet['original_value']} -> {packet['value']}")
                    done = True
            if done:
                break
        else:
            raise SystemExit("never received SetReply")

        print("OK")

    asyncio.run(run())


if __name__ == "__main__":
    main()
