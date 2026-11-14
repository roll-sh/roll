#!/usr/bin/env python3
"""Vector search benchmark: a deterministic corpus with exact ground truth, bulk indexing and
k-NN query workloads against roll search, Elasticsearch and OpenSearch
(docs/benchmarks/search.md, "Vector search").

    bench-search-vectors.py gen <dir> [--docs 1000000] [--dims 384] [--clusters 300]
                                      [--queries 200] [--seed 1]
        Writes <dir>/docs.json (one document per line: `v` a dense vector, `tag` a keyword,
        `ts` a date, `title` three words), <dir>/vectors.npy (the same vectors, float32, as
        the engines parse them), <dir>/queries.json (query vectors with their filters and
        match text) and <dir>/truth.json (exact top 100 by cosine for every query: no filter,
        the 10% filter and the 1% filter). Needs numpy.
    bench-search-vectors.py body <dialect> <type> <dims>
        Prints the index body: dialect es (roll, Elasticsearch) or os (OpenSearch); type
        default, hnsw, int8_hnsw or bbq_hnsw (OpenSearch: faiss HNSW fp32, Lucene HNSW with its
        `sq` encoder, faiss `on_disk` 32x).
    bench-search-vectors.py ingest <url> <dir> --index <name> --body <file> [--docs N]
                                   [--clients 8] [--bulk 2000] [--cgroup dir]
        A fresh index, bulks prepared in memory, sent by C clients, then a refresh: docs/s
        and server CPU per document (from the target's cgroup cpu.stat).
    bench-search-vectors.py search <url> <dir> --index <name> --dialect es|os
                                   --kind knn|knn_f10|knn_f1|rrf [--reps 200] [--warmup 30]
                                   [--cgroup dir] [--k 10] [--num-candidates 100]
        One client, the queries in order: latency p50/p99, recall@10 against the exact truth,
        server CPU per query. Prints one JSON line.

Vectors come from a Gaussian mixture (`clusters` centers of unequal weight, the points of
each spread along 16 directions of its own plus a little isotropic noise),
normalized and rounded to 4 decimals; the truth is computed on exactly the float32 values the
engines index, so recall is exact.
"""
import argparse
import json
import os
import queue
import sys
import threading
import time
import urllib.request

TAGS = 100  # `tag` values t00..t99, uniform: one is a 1% filter, ten a 10% filter


def gen(a):
    import numpy as np

    os.makedirs(a.dir, exist_ok=True)
    rng = np.random.default_rng(a.seed)
    d = a.dims
    centers = rng.standard_normal((a.clusters, d)).astype(np.float64)
    centers /= np.linalg.norm(centers, axis=1, keepdims=True)
    # Unequal cluster sizes (a Dirichlet draw), as real embeddings have.
    weights = rng.dirichlet(np.full(a.clusters, 2.0))
    words = ["w%04d" % i for i in range(2000)]
    topic = rng.integers(0, len(words), (a.clusters, 3))
    # Each cluster spreads mostly along its own 16 random directions (embeddings have a low
    # intrinsic dimension), plus a little isotropic noise.
    rank = 16
    basis = rng.standard_normal((a.clusters, rank, d)) / np.sqrt(d)

    def draw(n, r):
        c = r.choice(a.clusters, size=n, p=weights)
        z = r.standard_normal((n, rank)) * (0.7 / np.sqrt(rank))
        x = centers[c] + np.einsum("nr,nrd->nd", z, basis[c]) + r.standard_normal((n, d)) * (0.25 / np.sqrt(d))
        x /= np.linalg.norm(x, axis=1, keepdims=True)
        return c, np.round(x, 4)

    fmt = "[" + ",".join(["%.4f"] * d) + "]"
    t0 = 1_735_689_600  # 2025-01-01T00:00:00Z
    vecs = np.lib.format.open_memmap(os.path.join(a.dir, "vectors.npy"), mode="w+", dtype=np.float32, shape=(a.docs, d))
    tags = np.empty(a.docs, dtype=np.int16)
    with open(os.path.join(a.dir, "docs.json"), "w") as f:
        chunk = 50_000
        for start in range(0, a.docs, chunk):
            n = min(chunk, a.docs - start)
            c, x = draw(n, rng)
            vecs[start:start + n] = x.astype(np.float32)
            tg = rng.integers(0, TAGS, n)
            tags[start:start + n] = tg
            rw = rng.integers(0, len(words), (n, 1))
            tw = rng.integers(0, 3, (n, 2))
            lines = []
            for i in range(n):
                ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t0 + (start + i) * 30))
                title = "%s %s %s" % (words[topic[c[i], tw[i, 0]]], words[topic[c[i], tw[i, 1]]], words[rw[i, 0]])
                lines.append('{"v":%s,"tag":"t%02d","ts":"%s","title":"%s"}\n' % (fmt % tuple(x[i]), tg[i], ts, title))
            f.write("".join(lines))
            print("gen: %d / %d" % (start + n, a.docs), file=sys.stderr)
    vecs.flush()
    qrng = np.random.default_rng(a.seed + 1000)
    qc, q = draw(a.queries, qrng)
    q32 = q.astype(np.float32)
    queries = []
    for i in range(a.queries):
        t1 = int(qrng.integers(0, TAGS))
        t10 = int(qrng.integers(0, TAGS // 10)) * 10
        queries.append({
            "v": [float("%.4f" % v) for v in q[i]],
            "tag1": "t%02d" % t1,
            "tag10": ["t%02d" % t for t in range(t10, t10 + 10)],
            "text": "%s %s" % (words[topic[qc[i], 0]], words[topic[qc[i], 1]]),
        })
    # Exact cosine top 100, in float32 like the engines (vectors are unit length within
    # rounding; divide by the norms anyway).
    norms = np.linalg.norm(vecs, axis=1)
    qn = q32 / np.linalg.norm(q32, axis=1, keepdims=True)
    truth = {"knn": [], "knn_f10": [], "knn_f1": []}
    for i in range(a.queries):
        s = (vecs @ qn[i]) / norms
        masks = {
            "knn": None,
            "knn_f10": (tags >= int(queries[i]["tag10"][0][1:])) & (tags < int(queries[i]["tag10"][0][1:]) + 10),
            "knn_f1": tags == int(queries[i]["tag1"][1:]),
        }
        for kind, m in masks.items():
            ss = s if m is None else np.where(m, s, -np.inf)
            top = np.argpartition(-ss, 100)[:100]
            top = top[np.argsort(-ss[top], kind="stable")]
            truth[kind].append([int(t) for t in top])
    json.dump(queries, open(os.path.join(a.dir, "queries.json"), "w"))
    json.dump(truth, open(os.path.join(a.dir, "truth.json"), "w"))
    json.dump({"docs": a.docs, "dims": d, "clusters": a.clusters, "seed": a.seed}, open(os.path.join(a.dir, "meta.json"), "w"))


def body(dialect, kind, dims):
    props = {"tag": {"type": "keyword"}, "ts": {"type": "date"}, "title": {"type": "text"}}
    settings = {"index.number_of_shards": 1, "index.number_of_replicas": 0}
    if dialect == "es":
        v = {"type": "dense_vector", "dims": dims}
        if kind != "default":
            v["index_options"] = {"type": kind, "m": 16, "ef_construction": 100}
    else:
        settings["index.knn"] = True
        v = {"type": "knn_vector", "dimension": dims, "space_type": "cosinesimil"}
        if kind in ("default", "hnsw"):
            v["method"] = {"name": "hnsw", "engine": "faiss", "parameters": {"m": 16, "ef_construction": 100}}
        elif kind == "int8_hnsw":
            v["method"] = {"name": "hnsw", "engine": "lucene", "parameters": {"m": 16, "ef_construction": 100, "encoder": {"name": "sq", "parameters": {"bits": 7}}}}
        elif kind == "bbq_hnsw":
            v["mode"] = "on_disk"
            v["compression_level"] = "32x"
        if kind == "default":
            v = {"type": "knn_vector", "dimension": dims, "space_type": "cosinesimil"}
    props["v"] = v
    return {"settings": settings, "mappings": {"properties": props}}


class Client:
    def __init__(self, url):
        self.url = url.rstrip("/")

    def req(self, method, path, b=None, ct="application/json"):
        data = b if isinstance(b, (bytes, type(None))) else json.dumps(b).encode()
        r = urllib.request.Request(self.url + path, data=data, method=method, headers={"Content-Type": ct} if data else {})
        with urllib.request.urlopen(r, timeout=3600) as resp:
            return json.loads(resp.read() or b"null")


def cpu_us(cg):
    if not cg:
        return None
    for line in open(os.path.join(cg, "cpu.stat")):
        k, v = line.split()
        if k == "usage_usec":
            return int(v)


def ingest(a):
    c = Client(a.url)
    try:
        c.req("DELETE", "/" + a.index)
    except Exception:
        pass
    c.req("PUT", "/" + a.index, json.load(open(a.body)))
    bodies, batch = [], []
    with open(os.path.join(a.dir, "docs.json")) as f:
        for i, line in enumerate(f):
            if i == a.docs:
                break
            batch.append('{"index":{"_id":"%d"}}\n%s' % (i, line))
            if len(batch) == a.bulk:
                bodies.append("".join(batch).encode())
                batch = []
    if batch:
        bodies.append("".join(batch).encode())
    q = queue.Queue()
    for b in bodies:
        q.put(b)
    errors = []

    def work():
        while True:
            try:
                b = q.get_nowait()
            except queue.Empty:
                return
            r = c.req("POST", "/%s/_bulk" % a.index, b, "application/x-ndjson")
            if r.get("errors"):
                errors.append(json.dumps(r)[:400])

    c0 = cpu_us(a.cgroup)
    start = time.perf_counter()
    ts = [threading.Thread(target=work) for _ in range(a.clients)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    took = time.perf_counter() - start
    c1 = cpu_us(a.cgroup)
    c.req("POST", "/%s/_refresh" % a.index)
    refreshed = time.perf_counter() - start
    c2 = cpu_us(a.cgroup)
    n = c.req("GET", "/%s/_count" % a.index)["count"]
    out = {"clients": a.clients, "docs": n, "load_s": round(took, 1), "with_refresh_s": round(refreshed, 1),
           "docs_per_s": round(n / took), "docs_per_s_refreshed": round(n / refreshed)}
    if c0 is not None:
        out["cpu_us_per_doc"] = round((c1 - c0) / n, 1)
        out["cpu_us_per_doc_refreshed"] = round((c2 - c0) / n, 1)
    if errors:
        out["errors"] = errors[0]
    print(json.dumps(out))


def search_body(dialect, kind, q, k, nc):
    if dialect == "es":
        knn = {"field": "v", "query_vector": q["v"], "k": k, "num_candidates": nc}
        if kind == "knn_f10":
            knn["filter"] = {"terms": {"tag": q["tag10"]}}
        elif kind == "knn_f1":
            knn["filter"] = {"term": {"tag": q["tag1"]}}
        if kind == "rrf":
            return {"size": k, "_source": False, "retriever": {"rrf": {"retrievers": [
                {"standard": {"query": {"match": {"title": q["text"]}}}},
                {"knn": {"field": "v", "query_vector": q["v"], "k": k, "num_candidates": nc}},
            ], "rank_window_size": nc}}}
        return {"size": k, "_source": False, "knn": knn}
    v = {"vector": q["v"], "k": k, "method_parameters": {"ef_search": nc}}
    if kind == "knn_f10":
        v["filter"] = {"terms": {"tag": q["tag10"]}}
    elif kind == "knn_f1":
        v["filter"] = {"term": {"tag": q["tag1"]}}
    if kind == "rrf":
        return {"size": k, "_source": False, "query": {"hybrid": {"queries": [
            {"match": {"title": q["text"]}}, {"knn": {"v": v}}]}}}
    return {"size": k, "_source": False, "query": {"knn": {"v": v}}}


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def search(a):
    c = Client(a.url)
    queries = json.load(open(os.path.join(a.dir, "queries.json")))
    truth = json.load(open(os.path.join(a.dir, "truth.json"))).get(a.kind)
    path = "/%s/_search" % a.index
    if a.dialect == "os" and a.kind == "rrf":
        # OpenSearch's RRF is a search pipeline over a hybrid query.
        c.req("PUT", "/_search/pipeline/bench-rrf", {"phase_results_processors": [
            {"score-ranker-processor": {"combination": {"technique": "rrf", "rank_constant": 60}}}]})
        path += "?search_pipeline=bench-rrf"
    bodies = [json.dumps(search_body(a.dialect, a.kind, q, a.k, a.num_candidates)).encode() for q in queries]
    for i in range(a.warmup):
        c.req("POST", path, bodies[i % len(bodies)])
    lat, rec, tot = [], 0, 0
    dump = open(a.dump, "w") if a.dump else None
    c0 = cpu_us(a.cgroup)
    for i in range(a.reps):
        qi = i % len(bodies)
        t = time.perf_counter()
        r = c.req("POST", path, bodies[qi])
        lat.append((time.perf_counter() - t) * 1000)
        ids = [int(h["_id"]) for h in r["hits"]["hits"]]
        if dump:
            dump.write(json.dumps([[h["_id"], h["_score"]] for h in r["hits"]["hits"]]) + "\n")
        if truth is not None:
            want = set(truth[qi][:a.k])
            rec += len(want & set(ids[:a.k]))
            tot += a.k
    c1 = cpu_us(a.cgroup)
    out = {"kind": a.kind, "reps": a.reps, "p50_ms": round(pct(lat, 50), 2), "p99_ms": round(pct(lat, 99), 2),
           "mean_ms": round(sum(lat) / len(lat), 2)}
    if tot:
        out["recall"] = round(rec / tot, 4)
    if c0 is not None:
        out["cpu_ms_per_query"] = round((c1 - c0) / a.reps / 1000, 3)
    print(json.dumps(out))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gen")
    g.add_argument("dir")
    g.add_argument("--docs", type=int, default=1_000_000)
    g.add_argument("--dims", type=int, default=384)
    g.add_argument("--clusters", type=int, default=300)
    g.add_argument("--queries", type=int, default=200)
    g.add_argument("--seed", type=int, default=1)
    b = sub.add_parser("body")
    b.add_argument("dialect", choices=["es", "os"])
    b.add_argument("type", choices=["default", "hnsw", "int8_hnsw", "bbq_hnsw"])
    b.add_argument("dims", type=int)
    i = sub.add_parser("ingest")
    i.add_argument("url")
    i.add_argument("dir")
    i.add_argument("--index", required=True)
    i.add_argument("--body", required=True)
    i.add_argument("--docs", type=int, default=-1)
    i.add_argument("--clients", type=int, default=8)
    i.add_argument("--bulk", type=int, default=2000)
    i.add_argument("--cgroup")
    s = sub.add_parser("search")
    s.add_argument("url")
    s.add_argument("dir")
    s.add_argument("--index", required=True)
    s.add_argument("--dialect", choices=["es", "os"], default="es")
    s.add_argument("--kind", choices=["knn", "knn_f10", "knn_f1", "rrf"], default="knn")
    s.add_argument("--reps", type=int, default=200)
    s.add_argument("--warmup", type=int, default=30)
    s.add_argument("--k", type=int, default=10)
    s.add_argument("--num-candidates", type=int, default=100)
    s.add_argument("--cgroup")
    s.add_argument("--dump", help="write every response's (id, score) hits to this file, one line per query")
    a = ap.parse_args()
    if a.cmd == "gen":
        gen(a)
    elif a.cmd == "body":
        print(json.dumps(body(a.dialect, a.type, a.dims)))
    elif a.cmd == "ingest":
        ingest(a)
    else:
        search(a)


if __name__ == "__main__":
    main()
