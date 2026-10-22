"""Captures cartesian `shape` query results from Elasticsearch 9.5.5 (index `index-fx-xy`):
random points, lines, polygons (some with holes), multi-polygons and envelopes in a `shape`
field (indexed with doc values) and a doc-values-only one, multi-valued `point` fields (indexed with doc values, and doc values only), queried by
envelopes, polygons, points, multi-points, lines, multi-polygons and circles with every
relation; plus the profile descriptions of the first queries.
Usage: python3 -I gen_xy_shapes.py http://127.0.0.1:19200 out.json [seed]"""
import json, math, random, sys, urllib.request
ES = sys.argv[1].rstrip('/')
random.seed(int(sys.argv[3]) if len(sys.argv) > 3 else 11)

def req(method, path, body=None, nd=False):
    data = None
    headers = {}
    if body is not None:
        data = (body if nd else json.dumps(body)).encode()
        headers['content-type'] = 'application/x-ndjson' if nd else 'application/json'
    r = urllib.request.Request(ES + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            return json.loads(resp.read() or b'null')
    except urllib.error.HTTPError as e:
        return json.loads(e.read())

def R(a, b):
    # Mostly 3 decimals; some integers so boundaries coincide with query edges.
    v = random.uniform(a, b)
    return float(round(v)) if random.random() < 0.25 else round(v, 3)

def polar(cx, cy, n, rmin, rmax):
    pts = []
    for i in range(n):
        a = (i + random.uniform(-0.3, 0.3)) * 2 * math.pi / n
        r = random.uniform(rmin, rmax)
        pts.append([round(cx + r * math.cos(a), 3), round(cy + r * math.sin(a), 3)])
    return pts + [pts[0]]

def rand_geom(s):
    k = random.random()
    cx, cy = R(-100 * s, 100 * s), R(-100 * s, 100 * s)
    if k < 0.2:
        return {"type": "Point", "coordinates": [cx, cy]}
    if k < 0.35:
        n = random.choice([2, 3, 5])
        return {"type": "LineString", "coordinates": [[R(cx - 15 * s, cx + 15 * s), R(cy - 15 * s, cy + 15 * s)] for _ in range(n)]}
    if k < 0.7:
        rings = [polar(cx, cy, random.choice([3, 4, 6, 12]), 2 * s, 20 * s)]
        if random.random() < 0.3:
            rings = [polar(cx, cy, 8, 15 * s, 20 * s), polar(cx, cy, 5, 2 * s, 5 * s)]
        return {"type": "Polygon", "coordinates": rings}
    if k < 0.85:
        a = R(-100 * s, 100 * s); b = R(-100 * s, 100 * s)
        return {"type": "Envelope", "coordinates": [[a, b + R(1, 20 * s)], [a + R(1, 20 * s), b]]}
    return {"type": "MultiPolygon", "coordinates": [[polar(R(-100 * s, 100 * s), R(-100 * s, 100 * s), 5, 2 * s, 10 * s)] for _ in range(2)]}

docs = [rand_geom(1) for _ in range(220)] + [rand_geom(1e5) for _ in range(30)]
points = [[[R(-100, 100), R(-100, 100)] for _ in range(random.choice([1, 1, 2, 3]))] for _ in range(220)]
points += [[[R(-1e7, 1e7), R(-1e7, 1e7)]] for _ in range(30)]
queries = []
for i in range(60):
    g = rand_geom(1)
    if g["type"] == "Envelope":
        (a, b), (c, d) = g["coordinates"]
        g = {"type": "Envelope", "coordinates": [[a - 20, b + 30], [c + 30, d - 20]]}
    if g["type"] == "Polygon" and random.random() < 0.5:
        g = {"type": "Polygon", "coordinates": [polar(R(-70, 70), R(-70, 70), 7, 20, 45)]}
    queries.append(g)
for i in range(6):
    pts = [p for p in random.choice(points[:220])]
    queries.append({"type": "MultiPoint", "coordinates": pts + ([[R(-100, 100), R(-100, 100)]] if random.random() < 0.3 else [])})
for i in range(14):
    queries.append({"type": "Circle", "coordinates": [R(-90, 90), R(-90, 90)], "radius": str(random.choice([5, 20, 40, 80]))})
queries.append({"type": "Envelope", "coordinates": [[-1e8, 1e8], [1e8, -1e8]]})
queries.append({"type": "Envelope", "coordinates": [[-1e7, 1e7], [0, 0]]})
queries.append({"type": "Circle", "coordinates": [0, 0], "radius": "5000000"})

def main():
    out = {"docs": docs, "points": points, "cases": [], "descriptions": []}
    req("DELETE", "/index-fx-xy")
    m = {"s": {"type": "shape"}, "sn": {"type": "shape", "index": False}, "p": {"type": "point"}, "pn": {"type": "point", "index": False}}
    r = req("PUT", "/index-fx-xy", {"settings": {"number_of_shards": 1}, "mappings": {"properties": m}})
    assert r.get("acknowledged"), r
    lines = []
    for i, g in enumerate(docs):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"s": g, "sn": g, "p": points[i], "pn": points[i]}))
    r = req("POST", "/index-fx-xy/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    bad = [it["index"] for it in r["items"] if "error" in it["index"]]
    assert not bad, bad[:3]
    req("POST", "/index-fx-xy/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-xy/_refresh")
    for qi, g in enumerate(queries):
        for rel in ["intersects", "within", "disjoint", "contains"]:
            case = {"q": g, "relation": rel}
            for f in ["s", "sn", "p", "pn"]:
                body = {"query": {"shape": {f: {"shape": g, "relation": rel}}}, "size": 1000, "_source": False, "sort": ["_doc"]}
                if qi < 12:
                    body["profile"] = True
                r = req("POST", "/index-fx-xy/_search", body)
                if "error" in r:
                    rc = r["error"]["root_cause"][0]
                    case[f + "_error"] = [rc["type"], rc["reason"]]
                    continue
                case[f] = sorted(int(h["_id"]) for h in r["hits"]["hits"])
                if qi < 12:
                    case[f + "_description"] = r["profile"]["shards"][0]["searches"][0]["query"][0]["description"]
            out["cases"].append(case)
    req("DELETE", "/index-fx-xy")
    json.dump(out, open(sys.argv[2], "w"))
    print("ok", len(out["cases"]))

main()
