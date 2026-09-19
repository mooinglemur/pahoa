# Numbers on the wire

Archipelago's protocol is JSON, and its reference implementation is Python. Python's `int` is
unbounded and its `float` is an IEEE double, so the reference can hold any integer a client sends
and hands it back with every digit intact. Rust's JSON library cannot do that by default, and the
gap is not academic: it cost a live room a player's save state.

## What went wrong

A world packed its location checks into a single bitfield 71 bits wide and stored it through
`Set`. serde_json, built without `arbitrary_precision`, parses any integer literal that overflows
`u64` into an `f64`, **during parsing**, before the frame is dispatched. So by the time the room
saw the packet the digits were already gone:

```
stored: 2361183241434822606849
echoed: 2.3611832414348226e+21
```

No error was raised anywhere. The client wrote a bitfield and read back a number with its low
eleven bits replaced by zeros, and a restart rounded it a second time to a *different* wrong value,
because the save encoder is a separate trip through the same parser.

The louder half of the same bug: `or`, `and` and `xor` on such a value raised a Python-style
`TypeError` (the operand was a float by then, not an int), which drops the socket. A client
setting a bit in its own bitfield was disconnected for it.

## The fix, and what it costs

`serde_json`'s `arbitrary_precision` feature makes `Number` keep the literal text it was parsed
from. Integers of any width now survive parse, store, echo, save and restore byte for byte.

It is enabled in **`pahoa-proto`** (the crate that decodes and encodes frames) and again in
**`pahoa-datastore`**. The second is not redundant: cargo unifies features across a build, so
`pahoa-proto`'s flag covers every crate that depends on it, but `pahoa-datastore` does not, and
`cargo test -p pahoa-datastore` on its own would otherwise compile a serde_json that rounds a
bitfield to a float and measure a build nothing ships.

Three things change in exchange.

**`Number` equality becomes textual.** `0.0` and `-0.0` are now different numbers, and so are `1.5`
and `1.50`. Nothing in pahoa compares numbers that way (the data store has always used
`pyvalue::py_eq`, which reproduces Python's cross-type numeric equality), but a test written with
`assert_eq!(value, json!(…))` is now comparing spelling as well as value.

It also made the CPython vector suite stricter, and it immediately found a real divergence that had
been invisible for as long as the suite had existed: a floored modulo with a zero remainder takes
the sign of the **divisor**, so `0 % -2.5` is `-0.0`. CPython does this deliberately, because
platforms disagree about what `fmod` returns there. Five vectors had been passing on `0.0 == -0.0`.

**Floats keep the text the client wrote them with.** A client that sends `1.50` gets `1.50` back
where the reference would answer `1.5`. This is a divergence and it is deliberate: closing it means
walking every decoded value on every inbound frame to re-normalize its numbers, which is a cost on
the hot path for a difference no JSON parser can observe. Values pahoa *computes* are a different
matter and are rendered exactly as CPython's `repr` would render them. See below.

**Nothing parses faster.** `arbitrary_precision` stores numbers as strings, so reading one as an
integer parses it on demand. Frames are small and this has not been measurable, but it is the
reason the feature is not simply on everywhere by default.

## Rendering a float

`pyvalue::py_repr_f64` is the single place a float pahoa computed becomes JSON text, and
`slot_data` renders through it too so the two halves of the server agree with each other and with
Python. serde_json and CPython already emit the same *digits* (both produce the shortest string
that round-trips), but the layout differs twice:

| value | serde_json | CPython |
|---|---|---|
| `1e-5` | `0.00001` | `1e-05` |
| `1.5e-7` | `1.5e-7` | `1.5e-07` |
| `1e+16` | `1e+16` | `1e+16` |
| `5e-324` | `5e-324` | `5e-324` |

CPython switches to exponential notation below `1e-4` and pads an exponent to two digits. Positive
exponents and exponents of three digits or more already agree, since serde_json writes the `+` too.

## Arithmetic

`pyvalue::PyNum` holds a `num_bigint::BigInt`, so every operation is exact at any width: `or` sets
a bit in a 71-bit field, `left_shift` builds the mask, `add` does not round. This is the one place
in pahoa where a dependency beat hand-rolling: two's-complement bitwise operations over
sign-magnitude limbs, floored division, and shortest-round-trip decimal rendering of a
twenty-thousand-digit number are each a subtle algorithm, and being *exactly* right is the entire
point.

Two things needed care beyond swapping the type.

**Comparison does not go through a double.** Python compares an int with a float exactly rather
than converting, so `2**71 + 1` is **not** equal to the double it would round onto. Converting
first (which this module used to do) makes them equal, and then `remove` deletes the wrong
element of a list.

**Width is bounded, and the bound is about time rather than memory.** `ops::MAX_INT_BITS` is
65,536 bits: 8 KiB, just under 19,729 decimal digits, and 65,536 independent bit flags, an order of
magnitude past the largest world's location count. These operations run on the actor task, the one
thread that owns all room state, so a room that pauses for every player while somebody's tracker
multiplies two million-bit numbers is a worse failure than a refused `Set`. `pow`, `mul` and
`left_shift` decide from the *projected* width before allocating anything, which is the only way to
refuse `pow(2, 10**9)` at all: the reference server simply builds the 125 MB.

A value wider than the bound can still be stored and read back. Storage is verbatim passthrough and
costs nothing; it is arithmetic that is bounded.

## What the vectors say

`tools/gen-datastore-vectors.py` enumerates every operation against Archipelago's own
`modify_functions` table and records what CPython does. Its operand list now includes integers
either side of the `i64` and `u64` boundaries and at the reported 71-bit width, which took the
corpus from 13,122 cases to 19,602.

Every one of the 83 remaining divergences is the single documented `mod`-on-a-string case. There
are **no** width-bound divergences: every arithmetic case involving a wide integer now agrees with
CPython exactly, where the whole class used to be refused for not fitting in an `i64`.

The generator carries the same width bound, and for the same reason: with wide operands in the
matrix, `pow(2**71, 2**71)` is one of the pairs, and CPython does not fail on it. It takes the
machine down.
