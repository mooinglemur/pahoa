#!/usr/bin/env python3
"""Drive a running pahoa server with Archipelago's own client code.

This answers the one M4 question the Rust tests structurally cannot: the
synthetic test client never offers `permessage-deflate`, so it says nothing
about whether a *real* client — whose `websockets` library offers compression by
default — tolerates having it declined. That answer decides whether M8
(permessage-deflate) can be deferred or has to move up.

Run it against Archipelago's checkout so `CommonClient` and `NetUtils` are the
genuine articles rather than a reimplementation:

    ~/src/Archipelago/.venv/bin/python tools/real-client-check.py \\
        --archipelago ~/src/Archipelago \\
        --host localhost --port 38281 --slot "SomeName" --game "Some Game"

Exits non-zero on any failure, so it can gate a release.
"""

import argparse
import asyncio
import sys


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--archipelago", required=True)
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", type=int, default=38281)
    ap.add_argument("--slot", required=True)
    ap.add_argument("--game", required=True)
    ap.add_argument("--password", default=None)
    ap.add_argument(
        "--require-deflate",
        action="store_true",
        help="fail unless the server negotiated permessage-deflate with "
        "server_no_context_takeover and window bits 11 (M8's exit gate)",
    )
    ap.add_argument(
        "--check",
        type=int,
        action="append",
        default=[],
        help="location id to check; may be repeated",
    )
    args = ap.parse_args()

    sys.path.insert(0, args.archipelago)
    sys.argv = [sys.argv[0]]  # Archipelago's modules parse argv on import

    import ModuleUpdate

    # Skip the interactive dependency installer; the venv is already prepared.
    ModuleUpdate.update_ran = True

    import websockets
    from NetUtils import decode, encode

    print(f"using websockets {websockets.version.version} from {args.archipelago}")

    def text_parser(connected, data_package):
        """Archipelago's own `PrintJSON` renderer over a minimal client context.

        `NetUtils.RawJSONtoTextParser` is the real thing — its handler dispatch
        on part `type` is exactly what a wrong part type would break — but
        `CommonClient.CommonContext` is not usable here: importing it loads the
        world system, which this venv deliberately does not have. So the three
        lookups the handlers touch are supplied directly. They are reverse maps
        of the data package the server just sent, which is all
        `NameLookupDict` is (`CommonClient.py:238-292`).
        """
        from NetUtils import RawJSONtoTextParser

        class Lookup:
            def __init__(self, kind, by_game):
                self.kind, self.by_game = kind, by_game

            def lookup_in_slot(self, code, slot):
                game = slot_info[str(slot)].game if str(slot) in slot_info else "Archipelago"
                return self.by_game.get(game, {}).get(
                    code, f"Unknown {self.kind} (ID: {code})"
                )

        slot_info = connected["slot_info"]
        games = data_package["data"]["games"]

        class Ctx:
            slot = connected["slot"]
            player_names = {p.slot: p.name for p in connected["players"]} | {
                0: "Archipelago"
            }
            item_names = Lookup(
                "item",
                {g: {i: n for n, i in p["item_name_to_id"].items()} for g, p in games.items()},
            )
            location_names = Lookup(
                "location",
                {
                    g: {i: n for n, i in p["location_name_to_id"].items()}
                    for g, p in games.items()
                },
            )

            @staticmethod
            def slot_concerns_self(player):
                return player == Ctx.slot

        return RawJSONtoTextParser(Ctx())

    async def run():
        uri = f"ws://{args.host}:{args.port}"

        # Exactly how CommonClient connects (`CommonClient.py:872-874`):
        # default extensions, which means permessage-deflate IS offered.
        socket = await websockets.connect(
            uri, ping_timeout=None, ping_interval=None, max_size=16 * 1024 * 1024
        )

        # Report the *parameters*, not just the extension name. "PerMessageDeflate
        # was negotiated" is much weaker than what M8 needs to prove: that the
        # client accepted and applied `server_no_context_takeover`, which is what
        # makes a broadcast compressible once instead of once per connection.
        extensions = getattr(socket, "extensions", None) or []
        if not extensions:
            print("negotiated extensions: (none)")
        for extension in extensions:
            params = {
                name: getattr(extension, name, None)
                for name in (
                    "remote_no_context_takeover",
                    "local_no_context_takeover",
                    "remote_max_window_bits",
                    "local_max_window_bits",
                )
            }
            print(f"negotiated {type(extension).__name__}: {params}")
            if args.require_deflate:
                # From the client's point of view the server is "remote".
                if not params["remote_no_context_takeover"]:
                    raise SystemExit(
                        "server did not ask for no-context-takeover; a broadcast "
                        "would have to be compressed once per connection"
                    )
                if params["remote_max_window_bits"] != 11:
                    raise SystemExit(
                        f"expected server window bits 11, got "
                        f"{params['remote_max_window_bits']}"
                    )
        if args.require_deflate and not extensions:
            raise SystemExit("permessage-deflate was not negotiated")

        async def recv():
            raw = await asyncio.wait_for(socket.recv(), timeout=10)
            return decode(raw)

        async def send(packets):
            await socket.send(encode(packets))

        # 1. RoomInfo, unprompted.
        room_info = None
        for packet in await recv():
            if packet["cmd"] == "RoomInfo":
                room_info = packet
        assert room_info is not None, "expected RoomInfo first"
        print(f"RoomInfo: {len(room_info['games'])} games, seed {room_info['seed_name']}")
        print(f"  server version {room_info['version']}")

        # 2. DataPackage, decoded by Archipelago's own decoder.
        # No `games` key at all, which asks for everything. (An explicit empty
        # list means "no games" and is a different request; the reference server
        # treats it the same way.)
        await send([{"cmd": "GetDataPackage"}])
        data_package = None
        for _ in range(10):
            for packet in await recv():
                if packet["cmd"] == "DataPackage":
                    data_package = packet
            if data_package:
                break
        assert data_package is not None, "expected DataPackage"
        print(f"DataPackage: {len(data_package['data']['games'])} games")

        # 3. Connect, exactly as CommonClient does.
        import Utils

        await send(
            [
                {
                    "cmd": "Connect",
                    "password": args.password,
                    "name": args.slot,
                    "version": Utils.version_tuple,
                    "tags": ["AP"],
                    "items_handling": 0b111,
                    "uuid": Utils.get_unique_identifier(),
                    "game": args.game,
                    "slot_data": True,
                }
            ]
        )

        connected = None
        for _ in range(10):
            for packet in await recv():
                if packet["cmd"] == "ConnectionRefused":
                    raise SystemExit(f"connection refused: {packet['errors']}")
                if packet["cmd"] == "Connected":
                    connected = packet
            if connected:
                break
        assert connected is not None, "expected Connected"
        print(
            f"Connected: slot {connected['slot']}, "
            f"{len(connected['missing_locations'])} missing, "
            f"{len(connected['checked_locations'])} checked, "
            f"{len(connected['players'])} players"
        )

        # NetworkSlot / NetworkPlayer must have been reconstructed by the
        # allowlist in Archipelago's own decoder, not left as plain dicts.
        players = connected["players"]
        if players:
            p = players[0]
            assert hasattr(p, "name"), f"NetworkPlayer did not decode: {p!r}"
            print(f"  first player decoded as {type(p).__name__}: {p.name}")
        slot_info = connected["slot_info"]
        if slot_info:
            some = next(iter(slot_info.values()))
            assert hasattr(some, "game"), f"NetworkSlot did not decode: {some!r}"
            print(f"  slot_info decoded as {type(some).__name__}")

        # 4. Check locations, if asked.
        #
        # Rendered through Archipelago's own `JSONtoTextParser`, not by joining
        # the raw `text` fields. That is the whole point: the server sends bare
        # ids with `item_id`/`location_id`/`player_id` part types and the client
        # resolves them against its data package, so a raw join would print
        # "1 sent 42 to 2" and look fine while being unreadable to a human. If
        # the part types or the `player` field on them were wrong, the names
        # would come out as "Unknown item (ID: …)" here.
        if args.check:
            parser = text_parser(connected, data_package)

            await send([{"cmd": "LocationChecks", "locations": args.check}])
            saw_update = False
            unknown = []
            for _ in range(20):
                for packet in await recv():
                    if packet["cmd"] == "RoomUpdate" and "checked_locations" in packet:
                        print(f"RoomUpdate: checked {packet['checked_locations']}")
                        saw_update = True
                    if packet["cmd"] == "PrintJSON":
                        text = parser(packet["data"])
                        print(f"PrintJSON[{packet.get('type')}]: {text}")
                        if "Unknown item (ID:" in text or "Unknown location (ID:" in text:
                            unknown.append(text)
                if saw_update:
                    break
            assert saw_update, "location check produced no RoomUpdate"
            assert not unknown, f"ids did not resolve to names: {unknown}"

        await socket.close()
        print("OK")

    asyncio.run(run())


if __name__ == "__main__":
    main()
