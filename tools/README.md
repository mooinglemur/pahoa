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

Regenerate only when the reference changes; a diff in these files is a
behavioral change and deserves review.

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
also reports which WebSocket extensions were negotiated — that is what
established that clients tolerate a declined `permessage-deflate` — and renders
each `PrintJSON` through Archipelago's own `RawJSONtoTextParser`, so a wrong
message-part type shows up as `Unknown item (ID: …)` and fails the run rather
than passing as a plausible-looking string of digits.

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

- **`export-datapackage.py`** has been written and reviewed but never executed —
  it needs the full Archipelago import, which pulls in the world system. It is
  the only source of `hint_blacklist`, which exists in no multidata, so `!hint`
  cannot refuse non-hintable names without it. `pahoa serve` warns at startup
  when a snapshot is missing. Worth running before hints ship.
