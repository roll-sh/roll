#!/usr/bin/env python3
"""Writes src/stempel/stemmer_20000.bin, the Polish stemmer table of Lucene's
`lucene-analysis-stempel` (Apache-2.0; `org/apache/lucene/analysis/pl/stemmer_20000.tbl`, an
Egothor `MultiTrie2` in Java's DataOutput format) in a compact form read in place:

    u8 tries
    per trie:
      u16 root, u16 commands, u16 command_end[commands] (offsets into the UTF-8 bytes), bytes
      u16 rows, u32 row_end[rows] (cell indexes)
      cells: (u16 char, u16 command + 1, u16 row + 1), sorted by char within a row

Little endian. Only what `Trie.getLastOnPath` reads is kept (a cell's count and skip go).

    gen_stempel.py <stemmer_20000.tbl or lucene-analysis-stempel-*.jar>
"""
import os
import struct
import sys
import zipfile

src = sys.argv[1]
if src.endswith(".jar"):
    data = zipfile.ZipFile(src).read("org/apache/lucene/analysis/pl/stemmer_20000.tbl")
else:
    data = open(src, "rb").read()

pos = 0


def take(fmt):
    global pos
    v = struct.unpack_from(fmt, data, pos)
    pos += struct.calcsize(fmt)
    return v[0]


def utf():
    global pos
    n = take(">H")
    s = data[pos:pos + n]
    pos += n
    return s  # modified UTF-8 equals UTF-8 for these (no NUL, no supplementary characters)


method = utf().decode().upper()
assert "M" in method, method
forward = take(">?")
assert not forward
assert take(">i") == 1  # BY
out = bytearray()
tries = take(">i")
out += struct.pack("<B", tries)
for _ in range(tries):
    assert not take(">?")
    root = take(">i")
    cmds = [utf() for _ in range(take(">i"))]
    rows = []
    for _ in range(take(">i")):
        cells = []
        for _ in range(take(">i")):
            ch, cmd, _cnt, ref, _skip = take(">H"), take(">i"), take(">i"), take(">i"), take(">i")
            cells.append((ch, cmd + 1, ref + 1))
        rows.append(sorted(cells))
    out += struct.pack("<HH", root, len(cmds))
    end = 0
    for c in cmds:
        end += len(c)
        out += struct.pack("<H", end)
    out += b"".join(cmds)
    out += struct.pack("<H", len(rows))
    end = 0
    for r in rows:
        end += len(r)
        out += struct.pack("<I", end)
    for r in rows:
        for cell in r:
            out += struct.pack("<HHH", *cell)
assert pos == len(data)
dest = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "stempel", "stemmer_20000.bin")
open(dest, "wb").write(out)
print(f"{dest}: {len(out)} bytes")
