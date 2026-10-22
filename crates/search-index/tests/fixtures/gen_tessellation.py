import math, random, sys
random.seed(int(sys.argv[1]) if len(sys.argv) > 1 else 1)
out = []
def polar(cx, cy, n, rmin, rmax, jitter=0.3, ccw=True):
    pts = []
    for i in range(n):
        a = (i + random.uniform(-jitter, jitter)) * 2 * math.pi / n
        r = random.uniform(rmin, rmax)
        pts.append((round(cx + r * math.cos(a), 6), round(cy + r * math.sin(a), 6)))
    if not ccw:
        pts.reverse()
    return pts
def ring(pts):
    pts = pts + [pts[0]]
    return ",".join("%r %r" % p for p in pts)
for k in range(220):
    cx, cy = random.uniform(-150, 150), random.uniform(-60, 60)
    kind = k % 11
    check = random.random() < 0.5
    if kind < 5:
        n = random.choice([3, 4, 5, 7, 12, 30]) if kind < 4 else random.choice([81, 120, 200])
        line = ring(polar(cx, cy, n, 2, 20, ccw=random.random() < 0.5))
    elif kind < 7:
        n = random.choice([6, 10, 40, 90])
        outer = polar(cx, cy, n, 10, 20, ccw=random.random() < 0.5)
        holes = []
        for h in range(random.choice([1, 2, 3])):
            hx, hy = cx + random.uniform(-3, 3) + h * 0.01, cy + random.uniform(-3, 3)
            holes.append(polar(hx, hy, random.choice([3, 5, 8]), 0.5, 2.5, ccw=random.random() < 0.5))
        # keep holes disjoint: drop overlapping ones by spacing them
        holes = holes[:1] if len(holes) > 1 and random.random() < 0.5 else holes
        line = ring(outer) + "".join("|" + ring(h) for h in holes)
    elif kind == 7:
        # comb (concave)
        teeth = random.choice([3, 6, 25])
        w, h = 1.0, 5.0
        pts = [(cx, cy)]
        for t in range(teeth):
            x = cx + t * 2 * w
            pts += [(x, cy + h), (x + w, cy + h), (x + w, cy + 1), (x + 2 * w, cy + 1)]
        pts += [(cx + teeth * 2 * w, cy - 1)]
        pts = [(round(a, 6), round(b, 6)) for a, b in pts]
        line = ring(pts)
    elif kind == 8:
        # random points (often self-intersecting)
        pts = [(round(cx + random.uniform(-10, 10), 6), round(cy + random.uniform(-10, 10), 6)) for _ in range(random.choice([4, 5, 8]))]
        line = ring(pts)
    elif kind == 9:
        # square with collinear / duplicate points
        pts = [(cx, cy), (cx + 5, cy), (cx + 5, cy), (cx + 10, cy), (cx + 10, cy + 10), (cx + 5, cy + 10), (cx, cy + 10), (cx, cy + 5)]
        pts = [(round(a, 6), round(b, 6)) for a, b in pts]
        line = ring(pts)
    else:
        # hole sharing a vertex with the shell
        o = [(cx, cy), (cx + 10, cy), (cx + 10, cy + 10), (cx, cy + 10)]
        hh = [(cx, cy), (cx + 3, cy + 5), (cx + 5, cy + 3)]
        o = [(round(a, 6), round(b, 6)) for a, b in o]
        hh = [(round(a, 6), round(b, 6)) for a, b in hh]
        line = ring(o) + "|" + ring(hh)
    if any(abs(float(v.split()[1])) > 90 or abs(float(v.split()[0])) > 180 for r in line.split("|") for v in r.split(",")):
        continue
    out.append(("C " if check else "N ") + line)
print("\n".join(out))
