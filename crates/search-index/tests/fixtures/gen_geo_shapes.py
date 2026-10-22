"""Captures geo_shape query results from Elasticsearch 9.5.5 (index `index-fx-shape`):
random points, lines, polygons (some with holes), multi-polygons and envelopes, queried by
random envelopes (some across the dateline), polygons, points, lines and multi-polygons with
every relation. Usage: python3 -I gen_shapes.py http://127.0.0.1:19200 out.json [seed]"""
import json, math, random, sys, urllib.request
ES = sys.argv[1].rstrip('/')
random.seed(int(sys.argv[3]) if len(sys.argv) > 3 else 5)

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

R = lambda a, b: round(random.uniform(a, b), 5)

def polar(cx, cy, n, rmin, rmax):
    pts = []
    for i in range(n):
        a = (i + random.uniform(-0.3, 0.3)) * 2 * math.pi / n
        r = random.uniform(rmin, rmax)
        pts.append([round(cx + r * math.cos(a), 5), round(cy + r * math.sin(a), 5)])
    return pts + [pts[0]]

def rand_geom(region):
    (x0, x1, y0, y1) = region
    k = random.random()
    cx, cy = R(x0, x1), R(y0, y1)
    if k < 0.2:
        return {"type": "Point", "coordinates": [cx, cy]}
    if k < 0.35:
        n = random.choice([2, 3, 5])
        return {"type": "LineString", "coordinates": [[R(cx - 3, cx + 3), R(cy - 3, cy + 3)] for _ in range(n)]}
    if k < 0.7:
        rings = [polar(cx, cy, random.choice([3, 4, 6, 12]), 0.5, 4)]
        if random.random() < 0.3:
            rings = [polar(cx, cy, 8, 3, 4), polar(cx, cy, 5, 0.3, 1)]
        return {"type": "Polygon", "coordinates": rings}
    if k < 0.85:
        a = R(x0, x1); b = R(y0, y1)
        return {"type": "Envelope", "coordinates": [[a, b + R(0.1, 4)], [a + R(0.1, 4), b]]}
    return {"type": "MultiPolygon", "coordinates": [[polar(R(x0, x1), R(y0, y1), 5, 0.5, 2)] for _ in range(2)]}

REGION = (-20, 20, -20, 20)
docs = [rand_geom(REGION) for _ in range(250)]
# A few near the dateline.
docs += [{"type": "Polygon", "coordinates": [polar(R(172, 176), R(-5, 5), 6, 0.5, 3)]} for _ in range(6)]
docs += [{"type": "Point", "coordinates": [R(-179.9, -175), R(-5, 5)]} for _ in range(6)]

# geo_point values (one or several per doc).
points = [[[R(-20, 20), R(-20, 20)] for _ in range(random.choice([1, 1, 2, 3]))] for _ in docs]
queries = []
for i in range(70):
    g = rand_geom(REGION)
    if g["type"] == "Envelope":
        # Bigger boxes catch whole shapes.
        (a, b), (c, d) = g["coordinates"]
        g = {"type": "Envelope", "coordinates": [[a - 4, b + 6], [c + 6, d - 4]]}
    if g["type"] == "Polygon" and random.random() < 0.5:
        cx, cy = R(-15, 15), R(-15, 15)
        g = {"type": "Polygon", "coordinates": [polar(cx, cy, 7, 4, 9)]}
    queries.append(g)
for i in range(16):
    queries.append({"type": "Circle", "coordinates": [R(-18, 18), R(-18, 18)], "radius": "%dkm" % random.choice([50, 200, 400, 900])})
queries.append({"type": "Circle", "coordinates": [179.5, 0.5], "radius": "300km"})
queries.append({"type": "Envelope", "coordinates": [[170, 10], [-170, -10]]})
queries.append({"type": "Envelope", "coordinates": [[-20, 20], [20, -20]]})

def main():
    out = {"docs": docs, "points": points, "cases": [], "point_cases": []}
    req("DELETE", "/index-fx-shape")
    r = req("PUT", "/index-fx-shape", {"settings": {"number_of_shards": 1}, "mappings": {"properties": {"s": {"type": "geo_shape"}, "p": {"type": "geo_point"}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, g in enumerate(docs):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"s": g, "p": points[i]}))
    r = req("POST", "/index-fx-shape/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    bad = [it["index"] for it in r["items"] if "error" in it["index"]]
    assert not bad, bad[:3]
    req("POST", "/index-fx-shape/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-shape/_refresh")
    for g in queries:
        for rel in ["intersects", "within", "disjoint", "contains"]:
            body = {"query": {"geo_shape": {"s": {"shape": g, "relation": rel}}}, "size": 1000, "_source": False, "sort": ["_doc"]}
            r = req("POST", "/index-fx-shape/_search", body)
            if "error" in r:
                rc = r["error"]["root_cause"][0]
                out["cases"].append({"q": g, "relation": rel, "error": [rc["type"], rc["reason"]]})
                continue
            ids = sorted(int(h["_id"]) for h in r["hits"]["hits"])
            out["cases"].append({"q": g, "relation": rel, "ids": ids})
            body = {"query": {"geo_shape": {"p": {"shape": g, "relation": rel}}}, "size": 1000, "_source": False, "sort": ["_doc"]}
            r = req("POST", "/index-fx-shape/_search", body)
            if "error" in r:
                rc = r["error"]["root_cause"][0]
                out["point_cases"].append({"q": g, "relation": rel, "error": [rc["type"], rc["reason"]]})
                continue
            ids = sorted(int(h["_id"]) for h in r["hits"]["hits"])
            out["point_cases"].append({"q": g, "relation": rel, "ids": ids})
    req("DELETE", "/index-fx-shape")
    json.dump(out, open(sys.argv[2], "w"))
    print("ok", len(out["cases"]))

main()
