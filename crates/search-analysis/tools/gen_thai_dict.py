#!/usr/bin/env python3
"""Generates src/tokenizer/thai_words.bin, the built-in Thai word list of the `thai` tokenizer,
from ICU's Thai dictionary (icu4c/source/data/brkitr/dictionaries/thaidict.txt; Copyright
Unicode, Inc. and others, Unicode License v3, https://www.unicode.org/license.txt).

usage: python3 -I tools/gen_thai_dict.py <path to thaidict.txt> > src/tokenizer/thai_words.bin

Format: the words sorted by code point, front-coded. Each entry is one byte with the length of
the prefix shared with the previous word, then the remaining characters as one byte each
(code point - 0x0E00, all in 0x01..0x7F), then a 0x00 terminator.
"""
import sys

words = set()
for line in open(sys.argv[1], encoding='utf-8-sig'):
    w = line.split('#', 1)[0].split('\t', 1)[0].strip()
    if not w:
        continue
    for c in w:
        if not 0x0E01 <= ord(c) <= 0x0E7F:
            sys.exit(f'non-Thai character U+{ord(c):04X} in {w!r}')
    if len(w) > 255:
        sys.exit(f'word too long: {w!r}')
    words.add(w)
out = bytearray()
prev = ''
for w in sorted(words):
    n = 0
    while n < len(prev) and n < len(w) and prev[n] == w[n]:
        n += 1
    out.append(n)
    out.extend(ord(c) - 0x0E00 for c in w[n:])
    out.append(0)
    prev = w
sys.stdout.buffer.write(bytes(out))
print(f'{len(words)} words, {len(out)} bytes', file=sys.stderr)
