"""compare.py <engine.txt> <pg.txt>: the tests whose outcome differs between the engine and
PostgreSQL. Only these count: a test that fails on PostgreSQL too is the suite's problem."""
import sys


def load(path):
    d = {}
    with open(path) as f:
        for line in f:
            st, _, name = line.rstrip("\n").partition(" ")
            d[name] = st
    return d


eng, pg = load(sys.argv[1]), load(sys.argv[2])
diff = []
for name in sorted(set(eng) | set(pg)):
    e, p = eng.get(name, "MISSING"), pg.get(name, "MISSING")
    if e != p:
        diff.append((e, p, name))
same_pass = sum(1 for n in eng if eng[n] == "PASS" and pg.get(n) == "PASS")
print(f"both pass: {same_pass}, differ: {len(diff)}")
for e, p, name in diff:
    print(f"engine={e:<7} pg={p:<7} {name}")
