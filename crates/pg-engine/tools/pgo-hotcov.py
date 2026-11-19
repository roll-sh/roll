#!/usr/bin/env python3
"""How much of each PGO training class LLVM would treat as hot under a merged profile.

    tools/pgo-hotcov.py <classes dir> [<merged .profdata>...]

<classes dir> holds one profile per class, class-<name>.profdata (tools/dist-pgo.sh keeps them
in <out>/classes with PGO_KEEP=1). A merged profile's hot blocks are those whose count reaches
the count at which the blocks, largest first, make 99% of its total (LLVM's ProfileSummary hot
cutoff, 990000); code outside them is optimised as warm or cold. Per class, the line gives the
share of that class's own counts that lies in the merged profile's hot blocks. Without merged
profiles it reports the classes merged unweighted and merged with equal weight (dist-pgo.sh's).
"""
import array
import os
import subprocess
import sys


def profdata():
    root = subprocess.run(['rustc', '--print', 'sysroot'], capture_output=True, text=True, check=True).stdout.strip()
    vv = subprocess.run(['rustc', '-vV'], capture_output=True, text=True, check=True).stdout.splitlines()
    host = [l.split()[1] for l in vv if l.startswith('host:')][0]
    return os.path.join(root, 'lib', 'rustlib', host, 'bin', 'llvm-profdata')


def counters(path):
    """{function#hash: counters} of an indexed profile (its text form)."""
    text = subprocess.run([profdata(), 'merge', '--text', '-o', '-', path], capture_output=True, text=True, check=True).stdout
    out, name, mode, vals = {}, None, None, None
    for line in text.splitlines() + ['']:
        if not line:
            if name is not None and vals is not None:
                out[name] = vals
            name, mode, vals = None, None, None
        elif line.startswith('#'):
            mode = {'Func Hash': 'h', 'Counter Values': 'v'}.get(line[2:].split(':')[0])
            if mode == 'v':
                vals = array.array('q')
        elif line.startswith(':'):
            continue
        elif name is None:
            name = line
        elif mode == 'h':
            name += '#' + line
        elif mode == 'v' and line.lstrip('-').isdigit():
            vals.append(int(line))
    return out


def flat(profiles):
    keys = sorted(set().union(*profiles))
    width = {k: max(len(p.get(k, ())) for p in profiles) for k in keys}
    vecs = []
    for p in profiles:
        v = array.array('q')
        for k in keys:
            x = p.get(k, ())
            v.extend(x)
            v.extend([0] * (width[k] - len(x)))
        vecs.append(v)
    return vecs


def hot(m):
    s = sorted((x for x in m if x > 0), reverse=True)
    goal, acc = 0.99 * sum(s), 0
    for x in s:
        acc += x
        if acc >= goal:
            return [y >= x for y in m]
    return [False] * len(m)


def main():
    d = sys.argv[1]
    names = sorted(f[6:-9] for f in os.listdir(d) if f.startswith('class-') and f.endswith('.profdata'))
    merged = sys.argv[2:]
    vecs = flat([counters(os.path.join(d, f'class-{n}.profdata')) for n in names] + [counters(m) for m in merged])
    cls, given = vecs[:len(names)], vecs[len(names):]
    tots = [sum(v) for v in cls]
    rows = list(zip(merged, given))
    if not merged:
        top = max(tots)
        for label, ws in (('unweighted', [1] * len(cls)), ('equal weight', [max(1, int(top / t + 0.5)) for t in tots])):
            m = array.array('q', [0] * len(cls[0]))
            for w, v in zip(ws, cls):
                for i, x in enumerate(v):
                    m[i] += w * x
            rows.append((label, m))
    for label, m in rows:
        h = hot(m)
        cov = [100 * sum(x for x, y in zip(v, h) if y) / t if t else 0 for v, t in zip(cls, tots)]
        print(f'{label}: {sum(h)} hot blocks; ' + ', '.join(f'{n} {c:.2f}%' for n, c in zip(names, cov)))


if __name__ == '__main__':
    main()
