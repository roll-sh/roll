#!/usr/bin/env python3
"""Sum a heap profile of roll-engine (`--features heapprof`, ROLL_HEAPPROF=<file>).

    tools/heapprof.py <roll-engine binary> <file>.peak|<file>.live [--frames N] [--top N] [--by fn|stack]

Resolves each sampled stack with addr2line (build with CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
for inlined frames), and sums the sampled live bytes by the first frame inside pg_engine that
isn't an allocation helper (`--by fn`), or by the first N such frames (`--by stack`).
"""
import collections
import subprocess
import sys

SKIP = ("alloc::", "core::", "std::", "hashbrown::", "<alloc::", "<core::", "<std::", "<hashbrown::",
        "smallvec::", "<smallvec::", "__rust", "mimalloc", "heapprof", "_mi_", "mi_", "<T as ",
        "<[", "<&", "bytes::", "<bytes::", "indexmap", "<indexmap")


def main():
    binary, prof = sys.argv[1], sys.argv[2]
    args = sys.argv[3:]
    opt = lambda k, d: args[args.index(k) + 1] if k in args else d
    frames, top, by = int(opt("--frames", "4")), int(opt("--top", "40")), opt("--by", "fn")
    maps = prof.rsplit(".", 1)[0] + ".maps"
    base = None
    for line in open(maps):
        f = line.split()
        if len(f) >= 6 and f[5].endswith("roll-engine") and int(f[2], 16) == 0:
            base = int(f[0].split("-")[0], 16)
            break
    rows, header = [], ""
    for line in open(prof):
        if line.startswith("#"):
            header = line.strip()
            continue
        w, st = line.rstrip("\n").split("\t")
        rows.append((int(w), [int(a, 16) for a in st.split()]))
    addrs = sorted({a for _, st in rows for a in st})
    rel = {a: a - base for a in addrs if base is not None and a >= base}
    out = subprocess.run(["addr2line", "-a", "-f", "-C", "-i", "-e", binary] + [hex(rel[a] - 1) for a in addrs if a in rel],
                         capture_output=True, text=True).stdout.splitlines()
    sym, cur, i = {}, None, 0
    order = [a for a in addrs if a in rel]
    k = -1
    while i < len(out):
        if out[i].startswith("0x"):
            k += 1
            cur = order[k]
            sym[cur] = []
            i += 1
            continue
        fn = out[i]
        loc = out[i + 1] if i + 1 < len(out) else ""
        sym[cur].append((fn, loc))
        i += 2
    total = sum(w for w, _ in rows)
    agg = collections.Counter()
    for w, st in rows:
        names = []
        for a in st:
            for fn, loc in sym.get(a, [("?", "")]):
                # (Inlined frames have bare names: the standard library and other crates are
                # told by their files.)
                if fn.startswith(SKIP) or fn == "??" or "/rustc/" in loc or "/.cargo/" in loc:
                    continue
                names.append(f"{short(fn)} ({loc.split('/')[-1].split(' ')[0]})")
        key = names[0] if by == "fn" and names else " <- ".join(names[:frames]) or "?"
        agg[key] += w
    print(header, f"sampled {total / 2**20:.1f} MB")
    for key, w in agg.most_common(top):
        print(f"{w / 2**20:9.1f} MB {100 * w / max(total, 1):5.1f}%  {key}")


def short(fn):
    # Drop generic arguments and hashes: `pg_engine::exec::scan::Foo<T>::bar::h123` -> `exec::scan::Foo::bar`.
    out, depth = [], 0
    for ch in fn:
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth -= 1
        elif depth == 0:
            out.append(ch)
    s = "".join(out).replace("pg_engine::", "")
    return s.split("::{{closure}}")[0]


if __name__ == "__main__":
    main()
