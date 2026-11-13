#!/usr/bin/env python3
"""Replace the builder's profile name inside a release binary.

Rust's `--remap-path-prefix` covers the paths rustc itself embeds, but C
dependencies (`aws-lc`, `onig`, `mimalloc`, ...) bake `__FILE__` in at compile
time, so `C:\\Users\\<name>\\.cargo\\...` survives in the executable. Those
strings are only panic/diagnostic locations, and the replacement is
byte-for-byte the same length, so nothing in the PE layout moves.

    python scripts/strip-builder-paths.py dist.exe --name 51709 --to build

Prints how many occurrences were rewritten; exits non-zero if the lengths do
not match (which would mean the binary cannot be patched in place).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--name", required=True, help="Windows profile name to remove")
    parser.add_argument("--to", default="build", help="replacement, must be the same length")
    args = parser.parse_args()

    old = f"\\Users\\{args.name}".encode()
    new = f"\\Users\\{args.to}".encode()
    if len(old) != len(new):
        print(
            f"refusing to patch: {old!r} is {len(old)} bytes but {new!r} is {len(new)}",
            file=sys.stderr,
        )
        return 2

    data = bytearray(args.binary.read_bytes())
    count = data.count(old)
    if count == 0:
        print("nothing to patch")
        return 0

    data = data.replace(old, new)
    args.binary.write_bytes(bytes(data))
    print(f"patched {count} occurrences of {old!r} -> {new!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
