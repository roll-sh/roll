import collections
import re
import sys

# summ.py <crun output> : medians per step, engine (eng-N) against PostgreSQL (pg-N).
d = collections.defaultdict(lambda: collections.defaultdict(list))
extra = []
for line in open(sys.argv[1]):
    f = line.rstrip("\n").split("\t")
    if len(f) >= 4 and "wall" in line and (f[0].startswith("eng") or f[0].startswith("pg-")):
        side = "eng" if f[0].startswith("eng") else "pg"
        g = lambda k: re.search(k + r" ([\d.]+)", line)
        ins = g("instr")
        d[f[2]][side].append((float(g("wall").group(1)), float(g("cpu").group(1)), int(g("wrote").group(1)), float(ins.group(1)) if ins else None))
    elif "peak" in line or "autovac" in line:
        extra.append(line.strip())
med = lambda xs: sorted(xs)[len(xs) // 2]
print("| step | PostgreSQL wall / CPU | engine wall / CPU | engine instr (G) | wall ratio | CPU ratio | written MB (PG / engine) |")
print("|---|---|---|---|---|---|---|")
tot = [0.0, 0.0]
for s, v in d.items():
    if "pg" not in v or "eng" not in v:
        continue
    p = [med([x[i] for x in v["pg"]]) for i in range(3)]
    e = [med([x[i] for x in v["eng"]]) for i in range(3)]
    ins = [x[3] for x in v["eng"] if x[3] is not None]
    tot[0] += p[1]
    tot[1] += e[1]
    print(f"| {s} | {p[0]:.2f} / {p[1]:.2f} | {e[0]:.2f} / {e[1]:.2f} | {med(ins) if ins else '-'} | {e[0] / p[0]:.2f} | {e[1] / max(p[1], 0.005):.2f} | {p[2]} / {e[2]} |")
print(f"total CPU: PostgreSQL {tot[0]:.2f} s, engine {tot[1]:.2f} s, ratio {tot[1] / tot[0]:.2f}")
for x in extra:
    print(x)
