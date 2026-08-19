# Development-time tools

Everything here runs **at development or release time only**. None of it ships,
and the server never executes Python. The point of several of these is to
compare pahoa against Archipelago's *actual* implementation rather than against
a second reading of it — a transcription error in a reimplemented reference
would be copied into both sides and prove nothing.

## Setup

Most of these need an Archipelago checkout with a Python environment. A
virtualenv inside the checkout keeps it separate from the system Python:

```sh
cd ~/src/Archipelago
python3 -m venv .venv
.venv/bin/pip install websockets==13.1 colorama PyYAML schema jellyfish \
    orjson certifi platformdirs pathspec setuptools
```

That is the *client and server* subset — deliberately not `kivy` or the
generation stack, which none of this needs and which are slow to build. Scripts
set `ModuleUpdate.update_ran = True` to skip Archipelago's interactive
dependency installer.

A world failing to import (`zilliandomizer`, say) is harmless: nothing here
loads the world system.

## Generators — output is committed, so tests need no Python

| script | produces | pins |
|---|---|---|
| `gen-pyrandom-vectors.py` | `crates/pahoa-pyrandom/tests/vectors.txt` | CPython's Mersenne Twister: seeding, `getrandbits`, `_randbelow`, `shuffle`, state round-trip |
| `gen-protocol-vectors.py` | `crates/pahoa-proto/tests/vectors.txt` | Byte-exact server→client encodings, including the `"class"` tags |
| `gen-datastore-vectors.py` | `crates/pahoa-datastore/tests/vectors.jsonl` | All 18 `Set` operations, from Archipelago's real `modify_functions` |
| `gen-fuzzy-vectors.py` | `crates/pahoa-room/tests/fuzzy_vectors.jsonl` | `get_fuzzy_results` / `get_intended_text` scoring and thresholds |
| `gen-message-vectors.py` | `crates/pahoa-room/tests/message_vectors.jsonl` | `json_format_send_event` and `Hint.as_network_message`, byte for byte through `NetUtils.encode` |
| `gen-hint-vectors.py` | `crates/pahoa-room/tests/hint_vectors.jsonl` | `!hint` selection: candidates, spheres, cost accounting and reply text, from a real `MultiServer.Context` |

Regenerate only when the reference changes; a diff in these files is a
behavioral change and deserves review.

Two of these need arguments beyond `--archipelago`:

```sh
PYTHONHASHSEED=0 ~/src/Archipelago/.venv/bin/python tools/gen-hint-vectors.py \
    --archipelago ~/src/Archipelago \
    --multidata crates/pahoa-pickle/tests/fixtures/AP_56807069331869547085.archipelago \
    > crates/pahoa-room/tests/hint_vectors.jsonl
```

`PYTHONHASHSEED=0` only makes the *generating* run reproducible — the vectors
deliberately do not encode hint order, because Archipelago's own is not stable
across its own restarts (`Hint.__hash__` includes the entrance string, and
CPython randomizes string hashing per process). The vector carries the seed
name and the test refuses to run against a different fixture.

`gen-hint-vectors.py` is also the only generator that constructs a real
`MultiServer.Context`. It does that the way the reference itself does in
production: subclass and override `_load_game_data`, so the world system is
never imported.

## Differential checks — run against a live server

```sh
cargo build --release
./target/release/pahoa serve <seed>.archipelago --port 38281 &

~/src/Archipelago/.venv/bin/python tools/real-client-check.py \
    --archipelago ~/src/Archipelago --port 38281 \
    --slot "<slot name>" --game "<game>" \
    --check <location id> --check <location id>

~/src/Archipelago/.venv/bin/python tools/deathlink-check.py \
    --archipelago ~/src/Archipelago --port 38281 \
    --slot-a "<name>" --game-a "<game>" --slot-b "<name>" --game-b "<game>"
```

Both exit non-zero on failure, so they can gate a release. `real-client-check`
renders each `PrintJSON` through Archipelago's own `RawJSONtoTextParser`, so a
wrong message-part type shows up as `Unknown item (ID: …)` and fails the run
rather than passing as a plausible-looking string of digits.

It also reports the negotiated WebSocket extension **and its parameters**. At M4
that established clients tolerate a *declined* `permessage-deflate`; since M8 it
is the exit gate for the extension being right, and `--require-deflate` turns
the observation into a check:

```sh
~/src/Archipelago/.venv/bin/python tools/real-client-check.py \
    --archipelago ~/src/Archipelago --port 38281 --require-deflate \
    --slot "<slot name>" --game "<game>" --check <location id>
```

That fails unless the server asked for `server_no_context_takeover` and window
bits 11. The first of those is the load-bearing one: without it, identical
payloads compress to different bytes per connection and a broadcast costs one
compression per recipient instead of one per shard.

## Playing a whole seed — `play-seed.py`

M9's correctness half. Every player slot connects, checks every location it
owns, receives what it is owed, and claims its goal; then a final connection
audits the room — no missing locations, the checked count matches the multidata,
and `!status` agrees everyone is done.

```sh
~/src/Archipelago/.venv/bin/python tools/play-seed.py \
    --archipelago ~/src/Archipelago --port 38281 \
    --multidata crates/pahoa-pickle/tests/fixtures/<seed>.archipelago
```

Real in the ways that matter — Archipelago's `NetUtils.encode`/`decode`, its
`websockets` (so deflate is negotiated as a player's client would), its slot
metadata — and deliberately not a *game* client, since the server cannot tell
the difference and what is under test is the multiworld's completion rules.
`--slots N` plays only the first N for a quicker check. Exits non-zero on any
mismatch.

## Load testing — a separate track, and it has to be

The Python server cannot host 2000 slots at all, so differential testing proves
*fidelity* at small scale and can say nothing about *scale*.

```sh
cargo run --release -p pahoa-net --example loadtest -- \
    crates/pahoa-pickle/tests/fixtures/SYNTH_2000slot.archipelago 6000
```

Four phases — connect storm, steady mix, mass release cascade, reconnect storm —
against an in-process server, so the numbers the plan names are read directly
rather than inferred: actor mailbox depth, outbound bytes against the global
budget, lag disconnects, compressions, RSS.

The one to watch is **compressions against broadcasts**: it should track
broadcasts times *shards*, never times connections. A run where it approaches
the connection count means `server_no_context_takeover` did not negotiate.

Two traps this harness fell into, both worth knowing before writing another one:
clients must start reading the moment they connect (otherwise each accumulates
one join announcement per other connection, and the server rightly drops
connections that are not actually slow), and the load client must not inflate
what it receives (at 6000 connections the client-side inflate costs far more
than the server-side compression, so the harness becomes the bottleneck).

## WebSocket conformance — Autobahn

pahoa owns its WebSocket layer (`crates/pahoa-net/src/ws/`), because no crate
can send a *pre-compressed shared* frame and that is what one-broadcast-to-6000
requires. Owning it means proving it, which is what the Autobahn suite is for.

pahoa speaks Archipelago rather than echo, so conformance is measured against a
bare echo server exposing the same layer:

```sh
cargo run --release -p pahoa-net --example ws-echo -- 9001 &
docker run --rm --network host \
    -v "$PWD/tools/autobahn:/config:ro" -v "$PWD/target/autobahn:/reports" \
    crossbario/autobahn-testsuite \
    wstest -m fuzzingclient -s /config/fuzzingclient.json
```

The report lands in `target/autobahn/index.html`. Cases 1–10 cover framing,
fragmentation, UTF-8 handling and the close handshake; **12 and 13 are the
permessage-deflate suites** and are the ones M8 turned on. `Non-Strict` on a
performance case means a slow response rather than a wrong one; anything
reported as `Failed` is a real defect.

The location ids to pass to `--check` come from `pahoa inspect`, or from the
multidata's `locations` table for the slot you are connecting as.

## Fixtures and inspection

- `inspect-multidata.py` — reference implementation of `pahoa inspect`, compared
  line for line by `crates/pahoa/tests/inspect_differential.rs`.
- `dump-pickle.py` — canonical rendering of a pickle, compared byte for byte by
  `crates/pahoa-pickle/tests/fixtures.rs`.
- `make-large-fixture.py` — synthesizes the 2000-slot scale fixture. Explicitly
  synthetic: ids come from a real seed so names resolve, but placement is
  mechanical, so it is right for parse cost, `LocationStore` and fan-out, and
  wrong for anything about reachability.

Seeds are not committed. Put them in `crates/pahoa-pickle/tests/fixtures/`
(gitignored) or point `PAHOA_FIXTURE_DIR` elsewhere; tests skip loudly rather
than passing silently when they are absent.

## Not yet run

- **`export-datapackage.py`** regenerates pahoa's built-in `hint_blacklist`
  table from an Archipelago checkout. It needs one whose apworlds can *all*
  import, which a bare venv is not: 33 of them fail on missing dependencies
  here. Since its output deletes entries, it refuses to run rather than emit a
  silently short table. The current entries were established by reading the
  source instead — `grep -rn hint_blacklist worlds/` finds every one, since it
  is always a class-level assignment.
