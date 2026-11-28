#!/usr/bin/env python3
"""The code layout of a profile-guided build, for comparing two builds (HANDOFF "Reproducible PGO
training").

    tools/pgo-layout.py list <profdata> <roll-engine> [n]   > layout.txt
    tools/pgo-layout.py diff <layout A> <layout B>

list: the n hottest functions of the profile (default 300; by the sum of their counters, the
share of the profile's counts), each with its size in the binary's symbol table, or "inlined"
when the binary has no symbol of that name (LLVM inlined every call to it). One line each:
`<share %> <size | inlined> <symbol>`. tools/dist-pgo.sh writes it to <out>/layout.txt.

diff: the functions whose size or inline state differs between two lists, hottest first, and
the share of the counts they hold. Two trainings of one commit gave 114 of the 300 (27.8% of the
counts) with the recipe before 2026-11-28 and 85 (19.7%, by smaller amounts) with its fixed-work one. An
A/B pair built from one merged profile (dist-pgo.sh --profdata A --profdata B) gives the same
counts to every function the change did not touch: one of those in the diff is the change
moving inlining around it. Needs llvm-tools (llvm-profdata, llvm-nm: `rustup component add llvm-tools`).
"""
import os
import subprocess
import sys


def tool(name):
    host = subprocess.run(['rustc', '-vV'], capture_output=True, text=True, check=True).stdout
    host = next(l.split(': ')[1] for l in host.split('\n') if l.startswith('host: '))
    root = subprocess.run(['rustc', '--print', 'sysroot'], capture_output=True, text=True, check=True).stdout.strip()
    return os.path.join(root, 'lib', 'rustlib', host, 'bin', name)


def profile(path):
    """{symbol: count}: each function's counters summed (the name without its file prefix)."""
    text = subprocess.run([tool('llvm-profdata'), 'merge', '--text', '-o', '-', path],
                          capture_output=True, text=True, check=True).stdout.split('\n')
    out, i = {}, 0
    while i < len(text):
        line = text[i]
        if not line or line[0] in '#:':
            i += 1
            continue
        # name, '# Func Hash:', hash, '# Num Counters:', n, '# Counter Values:', counters...
        n = int(text[i + 4])
        name = line.split(';')[-1]
        out[name] = out.get(name, 0) + sum(int(x) for x in text[i + 6:i + 6 + n])
        i += 6 + n
        while i < len(text) and text[i] != '':
            i += 1
    return out


def sizes(binary):
    nm = subprocess.run([tool('llvm-nm'), '-S', '--defined-only', binary],
                        capture_output=True, text=True, check=True).stdout
    out = {}
    for line in nm.split('\n'):
        f = line.split()
        if len(f) == 4 and f[2] in 'tTwW':
            out[f[3]] = out.get(f[3], 0) + int(f[1], 16)
    return out


def read(path):
    rows = {}
    for line in open(path):
        share, size, sym = line.rstrip('\n').split(' ', 2)
        rows[sym] = (float(share), size)
    return rows


def main():
    if len(sys.argv) >= 4 and sys.argv[1] == 'list':
        counts = profile(sys.argv[2])
        have = sizes(sys.argv[3])
        n = int(sys.argv[4]) if len(sys.argv) > 4 else 300
        total = sum(counts.values()) or 1
        for sym, c in sorted(counts.items(), key=lambda kv: -kv[1])[:n]:
            print(f'{100 * c / total:.4f} {have.get(sym, "inlined")} {sym}')
    elif len(sys.argv) == 4 and sys.argv[1] == 'diff':
        a, b = read(sys.argv[2]), read(sys.argv[3])
        rows = [(max(a[s][0], b[s][0]), a[s][1], b[s][1], s) for s in a.keys() & b.keys() if a[s][1] != b[s][1]]
        rows.sort(reverse=True)
        names = '\n'.join(r[3] for r in rows)
        try:
            names = subprocess.run(['c++filt'], input=names, capture_output=True, text=True, check=True).stdout
        except (OSError, subprocess.CalledProcessError):
            pass
        for r, name in zip(rows, names.split('\n')):
            print(f'{r[0]:7.3f}% {r[1]:>8} {r[2]:>8}  {name[:150]}')
        print(f'{len(rows)} of {len(a.keys() & b.keys())} functions differ, '
              f'{sum(r[0] for r in rows):.2f}% of the counts')
        return 1 if rows else 0
    else:
        sys.exit(__doc__)
    return 0


if __name__ == '__main__':
    sys.exit(main())
