#!/usr/bin/env python3
"""Inject a freshly minted UUIDv7 `id = "..."` into every `#[orca_tool(...)]`
that lacks one. Idempotent: attributes that already carry an id are untouched.

Ids are minted once and committed; never re-run to "refresh" an id.
"""
import os
import re
import secrets
import sys
import time

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "projects")
SKIP_DIRS = {"target", "derive", "node_modules", ".git"}
ATTR = re.compile(r"#\[(?:[A-Za-z_][A-Za-z0-9_]*::)*orca_tool\(")

_last_ms = 0
_seq = 0


def mint_v7() -> str:
    """RFC 9562 UUIDv7 with a 12-bit counter so ids within one run are distinct and ordered."""
    global _last_ms, _seq
    ms = int(time.time() * 1000)
    if ms <= _last_ms:
        _seq += 1
        if _seq > 0xFFF:
            _last_ms += 1
            _seq = 0
        ms = _last_ms
    else:
        _last_ms, _seq = ms, 0
    rand_b = secrets.randbits(62)
    value = (ms << 80) | (0x7 << 76) | (_seq << 64) | (0b10 << 62) | rand_b
    h = f"{value:032x}"
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"


def attr_body(src: str, start: int) -> str:
    depth = 1
    i = start
    while depth:
        c = src[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif c == '"':
            i += 1
            while src[i] != '"':
                i += 2 if src[i] == "\\" else 1
        i += 1
    return src[start : i - 1]


def process(path: str) -> int:
    with open(path, encoding="utf-8") as f:
        src = f.read()
    out, pos, added = [], 0, 0
    for m in ATTR.finditer(src):
        line_start = src.rfind("\n", 0, m.start()) + 1
        if src[line_start : m.start()].lstrip().startswith("//"):
            continue
        body = attr_body(src, m.end())
        if body.lstrip().startswith("#") or re.search(r"(^|[,\s])id\s*=", body):
            continue
        out.append(src[pos : m.end()])
        out.append(f'id = "{mint_v7()}", ')
        pos = m.end()
        added += 1
    out.append(src[pos:])
    if added:
        with open(path, "w", encoding="utf-8") as f:
            f.write("".join(out))
    return added


def main() -> int:
    total = 0
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if name.endswith(".rs"):
                total += process(os.path.join(dirpath, name))
    print(f"minted {total} tool ids")
    return 0


if __name__ == "__main__":
    sys.exit(main())
