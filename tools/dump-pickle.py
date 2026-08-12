#!/usr/bin/env python3
"""Emit a canonical dump of an Archipelago pickle, for differential testing.

This is the Python half of pahoa-pickle's correctness gate: it renders a
`.archipelago` multidata (or a raw pickle) into a deterministic text form that
the Rust reader must reproduce byte for byte.

It deliberately does NOT import Archipelago. Reconstructing real `NetworkSlot`
and `Hint` objects would mean depending on Archipelago's runtime and would also
hide exactly the thing under test: the reader keeps class identity plus
positional args, so the reference must render the same shape rather than a
prettier one.

Usage:
    dump-pickle.py <file.archipelago | file.pickle> [--raw]

`--raw` treats the input as a bare pickle instead of the multidata container
(one format byte followed by zlib-compressed pickle).
"""

import io
import pickle
import sys
import zlib

# Mirrors pahoa_pickle::Allowlist::archipelago().
ALLOWED = {
    ("NetUtils", "NetworkItem"),
    ("NetUtils", "NetworkSlot"),
    ("NetUtils", "Hint"),
    ("NetUtils", "SlotType"),
    ("NetUtils", "HintStatus"),
    ("NetUtils", "ClientStatus"),
    ("collections", "Counter"),
}


class Instance:
    """Stand-in for a class-typed object, mirroring PyObj::Instance.

    Keeps the class name and positional arguments without importing anything.
    """

    __slots__ = ("cls", "args")

    def __init__(self, cls, args):
        self.cls = cls
        self.args = args


def _stub(module, name):
    """Build a callable that records its arguments instead of constructing."""

    class Stub:
        # REDUCE path: cls(*args)
        def __new__(cls, *args):
            return Instance(f"{module}.{name}", list(args))

        # NEWOBJ path: cls.__new__(cls, *args)
        @staticmethod
        def __getnewargs__():
            raise AssertionError("unreachable")

    Stub.__name__ = name
    Stub.__qualname__ = name
    return Stub


class StubUnpickler(pickle.Unpickler):
    def find_class(self, module, name):
        if (module, name) in ALLOWED:
            return _stub(module, name)
        raise pickle.UnpicklingError(f"global '{module}.{name}' is forbidden")


def load(data: bytes, raw: bool):
    if not raw:
        fmt = data[0]
        if fmt > 3:
            raise SystemExit(f"unsupported multidata format version {fmt}")
        data = zlib.decompress(data[1:])
    return StubUnpickler(io.BytesIO(data)).load()


def render(obj, out):
    """Write a canonical, unambiguous rendering of `obj` to `out`.

    Rules are chosen so both implementations can hit them exactly:
      - strings are length-prefixed rather than escaped, sidestepping any
        disagreement about escape sequences
      - floats are rendered as their IEEE-754 bit pattern, sidestepping any
        disagreement about shortest-round-trip formatting
      - sets are sorted by their own rendering, since Python sets do not
        preserve the pickle stream's order and pahoa's reader does
    """
    if obj is None:
        out.write("None")
    elif obj is True:
        out.write("True")
    elif obj is False:
        out.write("False")
    elif isinstance(obj, int):
        out.write(str(obj))
    elif isinstance(obj, float):
        import struct

        (bits,) = struct.unpack("<Q", struct.pack("<d", obj))
        out.write(f"f:{bits:016x}")
    elif isinstance(obj, str):
        # Length is in UTF-8 bytes so the prefix means the same thing on both
        # sides regardless of how each language counts string length.
        out.write(f"s{len(obj.encode('utf-8'))}:{obj}")
    elif isinstance(obj, tuple):
        out.write("(")
        for i, item in enumerate(obj):
            if i:
                out.write(",")
            render(item, out)
        out.write(")")
    elif isinstance(obj, list):
        out.write("[")
        for i, item in enumerate(obj):
            if i:
                out.write(",")
            render(item, out)
        out.write("]")
    elif isinstance(obj, (set, frozenset)):
        parts = []
        for item in obj:
            buf = io.StringIO()
            render(item, buf)
            parts.append(buf.getvalue())
        parts.sort()
        out.write("{" + ",".join(parts) + "}")
    elif isinstance(obj, dict):
        out.write("d{")
        for i, (k, v) in enumerate(obj.items()):
            if i:
                out.write(",")
            render(k, out)
            out.write(":")
            render(v, out)
        out.write("}")
    elif isinstance(obj, Instance):
        out.write(f"<{obj.cls}>(")
        for i, item in enumerate(obj.args):
            if i:
                out.write(",")
            render(item, out)
        out.write(")")
    else:
        raise SystemExit(f"cannot render {type(obj)!r}")


def main():
    args = [a for a in sys.argv[1:] if a != "--raw"]
    raw = "--raw" in sys.argv[1:]
    if len(args) != 1:
        raise SystemExit(__doc__)

    with open(args[0], "rb") as f:
        obj = load(f.read(), raw)

    buf = io.StringIO()
    render(obj, buf)
    sys.stdout.write(buf.getvalue())
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
