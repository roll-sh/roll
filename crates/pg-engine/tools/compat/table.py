"""table.py <results dir> <suite>...: per suite, the tests that ran on either server (skips aside),
how many had the same outcome on both, how many differ, and PostgreSQL's and roll's pass / fail."""
import pathlib
import sys


def load(path):
    d = {}
    if path.exists():
        for line in path.read_text(errors="replace").splitlines():
            st, _, name = line.partition(" ")
            if st in ("PASS", "FAIL"):
                d[name] = st
    return d


res = pathlib.Path(sys.argv[1])
print("| suite | tests | identical | differing | PostgreSQL pass / fail | roll pass / fail |")
print("|---|---:|---:|---:|---:|---:|")
for suite in sys.argv[2:]:
    pg, eng = load(res / f"{suite}-pg.txt"), load(res / f"{suite}-engine.txt")
    if not pg and not eng:
        print(f"| {suite} | not run | | | | |")
        continue
    names = set(pg) | set(eng)
    same = sum(1 for n in names if pg.get(n) == eng.get(n))

    def pf(d):
        return f"{sum(v == 'PASS' for v in d.values())} / {sum(v == 'FAIL' for v in d.values())}"

    print(f"| {suite} | {len(names)} | {same} | {len(names) - same} | {pf(pg)} | {pf(eng)} |")
