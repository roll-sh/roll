#!/usr/bin/env python3
"""Build maps.bin.zst, the encoding conversion tables of src/types/mbconv, from
Postgres's generated radix-tree maps (src/backend/utils/mb/Unicode/*.map).

    mkdir /tmp/pgmaps && cd /tmp/pgmaps
    for f in <the .map files named in MAPS below>; do
        curl -sSO https://raw.githubusercontent.com/postgres/postgres/REL_18_STABLE/src/backend/utils/mb/Unicode/$f
    done
    python3 gen_maps.py /tmp/pgmaps

Every radix tree is walked for all byte sequences it covers. A section of the blob is

    u8 name length, name, u8 arity, varint count, count rows of arity u32 columns

rows sorted by their columns; column 0 is stored as a delta from the previous row,
the others as zigzag deltas from the previous row's value. `<enc>_to_utf8` rows are
(local code, code point), `utf8_to_<enc>` rows (code point, local code); a local code is
its bytes big-endian in a u32, as Postgres packs it. `..._combined` sections hold the
*_2004 encodings' two-code-point characters: (local code, cp1, cp2) and (cp1, cp2, local).
"""
import os
import re
import sys
from compression import zstd

MAPS = [
    "big5", "euc_cn", "euc_jis_2004", "euc_jp", "euc_kr", "euc_tw", "gb18030", "gbk",
    "johab", "shift_jis_2004", "sjis", "uhc", "koi8r", "koi8u",
    "iso8859_2", "iso8859_3", "iso8859_4", "iso8859_5", "iso8859_6", "iso8859_7",
    "iso8859_8", "iso8859_9", "iso8859_10", "iso8859_13", "iso8859_14", "iso8859_15",
    "iso8859_16", "win866", "win874", "win1250", "win1251", "win1252", "win1253",
    "win1254", "win1255", "win1256", "win1257", "win1258",
]

NUM = re.compile(r"0x[0-9a-fA-F]+|\b\d+\b")


def strip_comments(s):
    return re.sub(r"/\*.*?\*/", "", s, flags=re.S)


def parse(path):
    src = open(path).read()
    body = strip_comments(src)
    tables = {}
    for m in re.finditer(r"static const uint(?:16|32) (\w+)\[\d+\]\s*=\s*\{(.*?)\};", body, re.S):
        tables[m.group(1)] = [int(x, 0) for x in NUM.findall(m.group(2))]
    m = re.search(r"static const pg_mb_radix_tree \w+\s*=\s*\{(.*?)\};", body, re.S)
    fields = [f.strip() for f in m.group(1).split(",")]
    chars = tables[fields[0]] if fields[0] != "NULL" else tables[fields[1]]
    v = [int(f, 0) for f in fields[2:]]
    combined = []
    for m in re.finditer(r"static const pg_\w+_combined \w+\[\d+\]\s*=\s*\{(.*?)\};", body, re.S):
        for row in re.findall(r"\{([^{}]*)\}", m.group(1)):
            combined.append(tuple(int(x, 0) for x in NUM.findall(row)))
    return chars, v, combined


def walk(chars, v):
    """All (packed input bytes, packed output) pairs of a pg_mb_radix_tree."""
    (b1root, b1l, b1u, b2root, b21l, b21u, b22l, b22u, b3root, b31l, b31u, b32l, b32u,
     b33l, b33u, b4root, b41l, b41u, b42l, b42u, b43l, b43u, b44l, b44u) = v
    out = {}
    if b1root:
        for b in range(b1l, b1u + 1):
            if (x := chars[b + b1root - b1l]):
                out[b] = x
    if b2root:
        for a in range(b21l, b21u + 1):
            i = chars[a + b2root - b21l]
            for b in range(b22l, b22u + 1):
                if (x := chars[b + i - b22l]):
                    out[a << 8 | b] = x
    if b3root:
        for a in range(b31l, b31u + 1):
            i = chars[a + b3root - b31l]
            for b in range(b32l, b32u + 1):
                j = chars[b + i - b32l]
                for c in range(b33l, b33u + 1):
                    if (x := chars[c + j - b33l]):
                        out[a << 16 | b << 8 | c] = x
    if b4root:
        for a in range(b41l, b41u + 1):
            i = chars[a + b4root - b41l]
            for b in range(b42l, b42u + 1):
                j = chars[b + i - b42l]
                for c in range(b43l, b43u + 1):
                    k = chars[c + j - b43l]
                    for d in range(b44l, b44u + 1):
                        if (x := chars[d + k - b44l]):
                            out[a << 24 | b << 16 | c << 8 | d] = x
    return out


def cp(word):
    """A code point from its UTF-8 bytes packed in a u32."""
    n = (word.bit_length() + 7) // 8
    return ord(word.to_bytes(n, "big").decode("utf-8"))


def varint(n, buf):
    while n >= 0x80:
        buf.append(n & 0x7F | 0x80)
        n >>= 7
    buf.append(n)


def section(buf, name, rows):
    rows = sorted(rows)
    buf.append(len(name))
    buf += name.encode()
    buf.append(len(rows[0]))
    varint(len(rows), buf)
    prev = [0] * len(rows[0])
    for r in rows:
        varint(r[0] - prev[0], buf)
        for i in range(1, len(r)):
            d = r[i] - prev[i]
            varint((d << 1) ^ (d >> 63) if d < 0 else d << 1, buf)
        prev = list(r)


def main():
    d = sys.argv[1]
    buf = bytearray()
    for enc in MAPS:
        chars, v, comb = parse(os.path.join(d, f"{enc}_to_utf8.map"))
        section(buf, f"{enc}_to_utf8", [(k, cp(x)) for k, x in walk(chars, v).items()])
        if comb:
            section(buf, f"{enc}_to_utf8_combined", [(c, cp(a), cp(b)) for c, a, b in comb])
        chars, v, comb = parse(os.path.join(d, f"utf8_to_{enc}.map"))
        section(buf, f"utf8_to_{enc}", [(cp(k), x) for k, x in walk(chars, v).items()])
        if comb:
            section(buf, f"utf8_to_{enc}_combined", [(cp(a), cp(b), c) for a, b, c in comb])
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "maps.bin.zst")
    data = zstd.compress(bytes(buf), level=22)
    open(out, "wb").write(data)
    print(f"{len(buf)} bytes raw, {len(data)} compressed -> {out}")


if __name__ == "__main__":
    main()
