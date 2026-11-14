#!/usr/bin/env python3
"""roll search micro benchmark: the same generated corpus and query sequences against several
Elasticsearch-compatible targets, interleaved.

    scripts/bench-search.py --target roll=http://127.0.0.1:19800 --target es=http://127.0.0.1:19210 \
        [--docs 50000] [--rounds 5] [--reps 200] [--out results.json] [--only match,sort_field]

Every round recreates the index on every target (bulk timed with the bodies already
serialized, so client-side JSON generation is not measured), then runs each query workload on
target A, target B, ... before moving to the next workload. Alternating at this granularity keeps
drift on a shared, busy host (load changes over minutes) from favouring one side. Each workload
draws its queries from a seeded generator, so every target answers the identical sequence.

Reported per workload and target: the median over rounds of the mean latency per request, the
min-max spread of those round means, and p50/p99 over every request of every round. With
`--cgroup name=/sys/fs/cgroup/...` (the target's container cgroup), also the server CPU time per
request (per bulk load), which a busy shared host distorts far less than wall time.
Python's standard library only.

    scripts/bench-search.py rally-summary <rally report csv>...

summarizes Rally races (`scripts/bench-search.sh rally`): files are named `...-<target>-<round>.csv`;
per task and target, the median over rounds of the p50 service time (indexing: median throughput),
with the min-max spread, and the error rate where it is not zero.

    scripts/bench-search.py ingest --target roll=... --target es=... --file docs.json --body index.json \
        [--docs 1000000] [--bulk 5000] [--clients 1,8,32] [--rounds 3] [--cgroup name=dir]

bulk indexing throughput, per-bulk service time and server CPU per document at each client count
(see `ingest`).

    scripts/bench-search.py rally-subset <local track dir> <rally data dir> <docs> [corpus regex]

cuts a local track copy's large corpora to their first <docs> documents (see `rally_subset`).
"""

import argparse
import http.client
import json
import random
import statistics
import sys
import threading
import time
import urllib.parse
import zlib

IDX = "perf-bench"
WORDS = ["w%d" % i for i in range(5000)]
CATS = ["c%d" % i for i in range(50)]
MAPPING = {
    "settings": {"number_of_shards": 1, "number_of_replicas": 0, "refresh_interval": "-1"},
    "mappings": {
        "properties": {
            "title": {"type": "text"},
            "body": {"type": "text"},
            "cat": {"type": "keyword"},
            "n": {"type": "long"},
            "price": {"type": "double"},
            "at": {"type": "date"},
        }
    },
}


class Client:
    def __init__(self, url):
        u = urllib.parse.urlparse(url)
        self.host, self.port = u.hostname, u.port or 9200
        self.conn = http.client.HTTPConnection(self.host, self.port, timeout=600)

    def req(self, method, path, body=None, ct="application/json"):
        if body is not None and not isinstance(body, (bytes, str)):
            body = json.dumps(body)
        if isinstance(body, str):
            body = body.encode()
        headers = {"Content-Type": ct} if body else {}
        for attempt in range(2):
            try:
                self.conn.request(method, path, body=body, headers=headers)
                r = self.conn.getresponse()
                return r.status, r.read()
            except (http.client.HTTPException, ConnectionError):
                if attempt:
                    raise
                self.conn.close()
                self.conn = http.client.HTTPConnection(self.host, self.port, timeout=600)


def zipf_word(rng):
    # A skewed vocabulary (like natural text): low ranks are frequent, so 2-3 term ORs mix
    # common and rare terms, which is what block-max pruning has to get right.
    r = int(rng.paretovariate(1.1)) - 1
    return WORDS[r % len(WORDS)] if rng.random() < 0.5 else rng.choice(WORDS)


def corpus(n, batch):
    rng = random.Random(7)
    bodies, cur = [], []
    for i in range(n):
        d = {
            "title": " ".join(zipf_word(rng) for _ in range(5)),
            "body": " ".join(zipf_word(rng) for _ in range(30)),
            "cat": rng.choice(CATS),
            "n": rng.randint(0, 100000),
            "price": round(rng.random() * 1000, 2),
            "at": 1700000000000 + rng.randint(0, 86400000 * 365),
        }
        cur.append('{"index":{"_id":"%d"}}\n%s\n' % (i, json.dumps(d)))
        if len(cur) == batch:
            bodies.append("".join(cur).encode())
            cur = []
    if cur:
        bodies.append("".join(cur).encode())
    return bodies


QUERIES = {
    "match": lambda r: {"query": {"match": {"body": r.choice(WORDS)}}},
    "match_or_2": lambda r: {"query": {"match": {"body": zipf_word(r) + " " + r.choice(WORDS)}}},
    "match_or_3": lambda r: {"query": {"match": {"body": " ".join(zipf_word(r) for _ in range(3))}}},
    "match_and_2": lambda r: {"query": {"match": {"body": {"query": zipf_word(r) + " " + zipf_word(r), "operator": "and"}}}},
    "bool_filter_range": lambda r: {
        "query": {"bool": {"must": [{"match": {"title": r.choice(WORDS)}}], "filter": [{"term": {"cat": r.choice(CATS)}}, {"range": {"n": {"gte": 10000}}}]}}
    },
    "terms_agg": lambda r: {"size": 0, "aggs": {"c": {"terms": {"field": "cat"}, "aggs": {"p": {"avg": {"field": "price"}}}}}},
    "date_histogram": lambda r: {"size": 0, "aggs": {"h": {"date_histogram": {"field": "at", "calendar_interval": "month"}}}},
    "sort_field": lambda r: {"query": {"match": {"body": r.choice(WORDS)}}, "sort": [{"n": "desc"}]},
    "sort_match_all": lambda r: {"sort": [{"n": "desc"}]},
    "range_count": lambda r: {"size": 0, "track_total_hits": True, "query": {"range": {"price": {"gte": 100, "lt": 900}}}},
    "match_all_count": lambda r: {"size": 0, "track_total_hits": True, "query": {"match_all": {}}},
}
# Workloads also run with `request_cache=false` (`<name> (no cache)`).
NO_CACHE = ["terms_agg", "date_histogram", "range_count", "match_all_count"]


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return 0.0
    k = min(len(xs) - 1, max(0, int(round(p / 100 * (len(xs) - 1)))))
    return xs[k]


def bulk_parallel(url, bodies, clients):
    """Sends `bodies` from `clients` threads (bodies dealt round-robin); returns seconds."""
    errors = []

    def run(part):
        c = Client(url)
        for b in part:
            s, out = c.req("POST", "/%s/_bulk" % IDX, b, "application/x-ndjson")
            if s != 200 or b'"errors":true' in out[:64]:
                errors.append((s, out[:300]))

    parts = [bodies[i::clients] for i in range(clients)]
    threads = [threading.Thread(target=run, args=(p,)) for p in parts]
    t = time.perf_counter()
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    el = time.perf_counter() - t
    if errors:
        raise SystemExit("bulk failed on %s: %r" % (url, errors[0]))
    return el


def rally_summary(files):
    import csv
    import os

    per = {}  # (task, metric) -> target -> [values]
    errors = {}
    targets = []
    for f in files:
        target = os.path.basename(f).rsplit(".", 1)[0].split("-")[-2]
        if target not in targets:
            targets.append(target)
        with open(f) as fh:
            for row in csv.reader(fh):
                if len(row) < 4 or not row[1]:
                    continue
                metric, task, value, unit = row[0], row[1], row[2], row[3]
                try:
                    v = float(value)
                except ValueError:
                    continue
                if metric == "error rate":
                    errors.setdefault(task, {}).setdefault(target, []).append(v)
                elif metric == "50th percentile service time" or (metric == "Median Throughput" and unit == "docs/s"):
                    per.setdefault((task, metric, unit), {}).setdefault(target, []).append(v)
    print("| task | metric | " + " | ".join("%s median (min-max)" % t for t in targets) + " |")
    print("|---|---|" + "---:|" * len(targets))
    for (task, metric, unit), by in per.items():
        cells = []
        for t in targets:
            xs = by.get(t, [])
            err = errors.get(task, {}).get(t, [])
            e = " err %.0f%%" % statistics.median(err) if err and statistics.median(err) > 0 else ""
            cells.append("%.2f (%.2f-%.2f)%s" % (statistics.median(xs), min(xs), max(xs), e) if xs else "-")
        name = "p50 service time (%s)" % unit if metric.startswith("50th") else "median throughput (%s)" % unit
        print("| %s | %s | %s |" % (task, name, " | ".join(cells)))


def rally_subset(track_dir, data_dir, limit, only=""):
    """Points a local track copy's large corpora at their first `limit` documents.

    Rally decompresses a whole corpus file before it indexes any part of it (nyc_taxis, pmc, so
    and the vector tracks are 10-80 GB uncompressed), so `ingest_percentage` doesn't save any
    scratch space. Instead, every document entry with more than `limit` documents (and, with
    `only`, a `<corpus>/<source-file>` matching that regex) is streamed once from its base URL
    through the decompressor and cut after `limit` lines into `<data_dir>/<corpus>/<name>-<limit>.json`
    (only the needed prefix is downloaded), and the entry is rewritten to that uncompressed file
    with its real size, so Rally finds it ready and verifies it."""
    import glob
    import os
    import re
    import subprocess

    entry = re.compile(r'\{[^{}]*"source-file"\s*:\s*"([^"]+)"[^{}]*\}')
    for path in glob.glob(os.path.join(track_dir, "**", "*.json"), recursive=True):
        with open(path) as fh:
            text = fh.read()
        if '"corpora"' not in text:
            continue
        out, pos = [], 0
        for m in entry.finditer(text):
            try:
                doc = json.loads(m.group(0))
            except ValueError:
                continue  # a templated entry: leave it alone
            head = text[: m.start()]
            names = re.findall(r'"name"\s*:\s*"([^"]+)"', head)
            bases = re.findall(r'"base-url"\s*:\s*"([^"]+)"', head)
            src = doc["source-file"]
            if doc.get("document-count", 0) <= limit or not names or not bases:
                continue
            if only and not re.search(only, "%s/%s" % (names[-1], src)):
                continue
            stem = re.sub(r"\.(bz2|zst|gz)$", "", src)
            sub = re.sub(r"(\.json)?$", "-%d.json" % limit, stem, count=1)
            dest = os.path.join(data_dir, names[-1], sub)
            if not os.path.exists(dest):
                os.makedirs(os.path.dirname(dest), exist_ok=True)
                unpack = {"bz2": "bzip2 -dc", "zst": "zstd -dc", "gz": "gzip -dc"}.get(src.rsplit(".", 1)[-1], "cat")
                url = "%s/%s" % (bases[-1].rstrip("/"), src)
                print("rally-subset: %s -> %s" % (url, dest), file=sys.stderr)
                # head closes the pipe after `limit` lines; curl and the decompressor then die of
                # SIGPIPE, which is the point: only the prefix is downloaded.
                subprocess.run("curl -sfL %s | %s | head -n %d > %s.part" % (url, unpack, limit, dest), shell=True, check=False)
                with open(dest + ".part", "rb") as fh:
                    lines = sum(chunk.count(b"\n") for chunk in iter(lambda: fh.read(1 << 24), b""))
                if lines != limit:
                    raise SystemExit("rally-subset: %s has %d lines, expected %d" % (dest, lines, limit))
                os.rename(dest + ".part", dest)
            doc["source-file"] = sub
            doc["document-count"] = limit
            doc.pop("compressed-bytes", None)
            doc["uncompressed-bytes"] = os.path.getsize(dest)
            out.append(text[pos : m.start()])
            out.append(json.dumps(doc))
            pos = m.end()
        if out:
            out.append(text[pos:])
            with open(path, "w") as fh:
                fh.write("".join(out))


def ingest(argv):
    """Bulk indexing at several client counts, interleaved between targets.

    Reads `--docs` documents (one JSON per line, e.g. a Rally corpus file), builds `--bulk`-doc
    `_bulk` bodies once, then per round and client count recreates `--index` on each target
    (body from `--body`), sends every body from that many threads and refreshes. Reports
    throughput, per-bulk service time percentiles and, with `--cgroup`, server CPU per document
    (sampled again after `--settle` seconds so merges that finish after the load count too)."""
    ap = argparse.ArgumentParser(prog="bench-search.py ingest")
    ap.add_argument("--target", action="append", required=True, help="name=url")
    ap.add_argument("--file", required=True)
    ap.add_argument("--body", required=True, help="index creation body (JSON file)")
    ap.add_argument("--index", default="perf-bench-ingest")
    ap.add_argument("--docs", type=int, default=1000000)
    ap.add_argument("--bulk", type=int, default=5000)
    ap.add_argument("--clients", default="1,8,32")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--settle", type=float, default=5.0)
    ap.add_argument("--cgroup", action="append", default=[], help="name=cgroup dir")
    ap.add_argument("--out", default="")
    ap.add_argument("--keep", action="store_true", help="keep the index after the last load")
    a = ap.parse_args(argv)
    targets = [(t.split("=", 1)[0], t.split("=", 1)[1].rstrip("/")) for t in a.target]
    cgroups = dict(c.split("=", 1) for c in a.cgroup)
    with open(a.body) as f:
        body = json.load(f)
    bodies, cur, n = [], [], 0
    with open(a.file, "rb") as f:
        for line in f:
            if n == a.docs:
                break
            cur.append(b'{"index":{}}\n' + line.rstrip(b"\n") + b"\n")
            n += 1
            if len(cur) == a.bulk:
                bodies.append(b"".join(cur))
                cur = []
    if cur:
        bodies.append(b"".join(cur))

    def cpu_ms(name):
        d = cgroups.get(name)
        if not d:
            return None
        with open(d + "/cpu.stat") as f:
            return next(int(l.split()[1]) for l in f if l.startswith("usage_usec")) / 1000.0

    results = {}  # (clients, target) -> [round dicts]
    for rnd in range(a.rounds):
        for clients in [int(c) for c in a.clients.split(",")]:
            order = targets if rnd % 2 == 0 else targets[::-1]
            for name, url in order:
                c = Client(url)
                c.req("DELETE", "/" + a.index)
                s, out = c.req("PUT", "/" + a.index, body)
                assert s == 200, (name, s, out[:300])
                lat, errors, lock = [], [], threading.Lock()
                queue = list(range(len(bodies)))

                def run():
                    cl = Client(url)
                    while True:
                        with lock:
                            if not queue:
                                return
                            i = queue.pop(0)
                        t = time.perf_counter()
                        s, out = cl.req("POST", "/%s/_bulk" % a.index, bodies[i], "application/x-ndjson")
                        dt = (time.perf_counter() - t) * 1000
                        with lock:
                            lat.append(dt)
                            if s != 200 or b'"errors":true' in out[:64]:
                                errors.append((s, out[:300]))

                c0 = cpu_ms(name)
                threads = [threading.Thread(target=run) for _ in range(clients)]
                t0 = time.perf_counter()
                for th in threads:
                    th.start()
                for th in threads:
                    th.join()
                load = time.perf_counter() - t0
                c1 = cpu_ms(name)
                t = time.perf_counter()
                c.req("POST", "/%s/_refresh" % a.index)
                refresh = (time.perf_counter() - t) * 1000
                time.sleep(a.settle)
                c2 = cpu_ms(name)
                if errors:
                    raise SystemExit("bulk failed on %s: %r" % (name, errors[0]))
                r = {"docs_s": n / load, "p50": pct(lat, 50), "p90": pct(lat, 90), "p99": pct(lat, 99), "refresh_ms": refresh}
                if c0 is not None:
                    r["cpu_us_doc"] = (c1 - c0) * 1000 / n
                    r["cpu_us_doc_settled"] = (c2 - c0) * 1000 / n
                r["stats"] = json.loads(c.req("GET", "/%s/_stats/merge,refresh,flush,segments" % a.index)[1])["_all"]["primaries"]
                results.setdefault((clients, name), []).append(r)
                print("round %d %-5s %2d clients: %.0f docs/s p50 %.1f p90 %.1f p99 %.1f ms refresh %.0f ms%s" % (
                    rnd, name, clients, r["docs_s"], r["p50"], r["p90"], r["p99"], refresh,
                    " cpu %.1f/%.1f us/doc" % (r["cpu_us_doc"], r["cpu_us_doc_settled"]) if "cpu_us_doc" in r else ""), file=sys.stderr, flush=True)
                if not a.keep:
                    c.req("DELETE", "/" + a.index)
    keys = ["docs_s", "p50", "p90", "p99", "refresh_ms", "cpu_us_doc", "cpu_us_doc_settled"]
    print("| clients | target | " + " | ".join(keys) + " |")
    print("|---|---|" + "---:|" * len(keys))
    for (clients, name), rs in results.items():
        cells = []
        for k in keys:
            xs = [r[k] for r in rs if k in r]
            cells.append("%.1f (%.1f-%.1f)" % (statistics.median(xs), min(xs), max(xs)) if xs else "-")
        print("| %d | %s | %s |" % (clients, name, " | ".join(cells)))
    if a.out:
        with open(a.out, "w") as f:
            json.dump({"%d-%s" % k: v for k, v in results.items()}, f, indent=1)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "rally-summary":
        return rally_summary(sys.argv[2:])
    if len(sys.argv) > 1 and sys.argv[1] == "ingest":
        return ingest(sys.argv[2:])
    if len(sys.argv) > 1 and sys.argv[1] == "rally-subset":
        a = sys.argv[2:]
        return rally_subset(a[0], a[1], int(a[2]), a[3] if len(a) > 3 else "")
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", action="append", required=True, help="name=url")
    ap.add_argument("--docs", type=int, default=50000)
    ap.add_argument("--batch", type=int, default=1000)
    ap.add_argument("--bulk-clients", type=int, default=1)
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--reps", type=int, default=200)
    ap.add_argument("--warmup", type=int, default=30)
    ap.add_argument("--only", default="", help="comma-separated workloads (bulk, get, query names)")
    ap.add_argument("--out", default="")
    ap.add_argument("--keep", action="store_true", help="keep the index at the end")
    ap.add_argument("--reuse", action="store_true", help="query the index a --keep run left (no bulk)")
    ap.add_argument("--cgroup", action="append", default=[], help="name=cgroup dir: report server CPU time")
    a = ap.parse_args()
    targets = []
    for t in a.target:
        name, url = t.split("=", 1)
        targets.append((name, url.rstrip("/")))
    only = set(filter(None, a.only.split(",")))
    want = lambda w: not only or w in only or w.split(" ")[0] in only

    bodies = corpus(a.docs, a.batch) if not a.reuse else []
    bulk_name = "bulk %dk docs, %d client%s" % (a.docs // 1000, a.bulk_clients, "s" if a.bulk_clients > 1 else "")
    rounds = {}  # workload -> target -> [round mean ms]
    lat = {}  # workload -> target -> [request ms]
    cpu = {}  # workload -> target -> [server CPU ms per request (per bulk load)]
    cgroups = dict(c.split("=", 1) for c in a.cgroup)

    def cpu_ms(name):
        d = cgroups.get(name)
        if not d:
            return None
        with open(d + "/cpu.stat") as f:
            for line in f:
                k, v = line.split()
                if k == "usage_usec":
                    return int(v) / 1000.0
        return None

    def record_cpu(w, name, c0, n):
        c1 = cpu_ms(name)
        if c0 is not None and c1 is not None:
            cpu.setdefault(w, {}).setdefault(name, []).append((c1 - c0) / n)

    def record(w, t, mean, reqs):
        rounds.setdefault(w, {}).setdefault(t, []).append(mean)
        lat.setdefault(w, {}).setdefault(t, []).extend(reqs)

    for rnd in range(a.rounds):
        for name, url in targets if not a.reuse else []:
            c = Client(url)
            c.req("DELETE", "/" + IDX)
            s, out = c.req("PUT", "/" + IDX, MAPPING)
            assert s == 200, (name, s, out)
            c0 = cpu_ms(name)
            el = bulk_parallel(url, bodies, a.bulk_clients)
            t = time.perf_counter()
            s, _ = c.req("POST", "/%s/_refresh" % IDX)
            el += time.perf_counter() - t
            assert s == 200
            record_cpu(bulk_name, name, c0, 1)
            record(bulk_name, name, el * 1000, [])
            print("round %d %-6s %s: %.2fs (%.0f docs/s)" % (rnd, name, bulk_name, el, a.docs / el), file=sys.stderr, flush=True)
        work = [(w, QUERIES[w], "") for w in QUERIES] + [(w + " (no cache)", QUERIES[w], "?request_cache=false") for w in NO_CACHE]
        for w, mk, qs in work:
            if not want(w):
                continue
            seed = zlib.crc32(w.encode()) & 0xFFFF
            for name, url in targets:
                c = Client(url)
                rng = random.Random(seed)
                for _ in range(a.warmup):
                    c.req("POST", "/%s/_search%s" % (IDX, qs), mk(rng))
                rng = random.Random(seed * 31 + rnd)
                reqs = [json.dumps(mk(rng)).encode() for _ in range(a.reps)]
                ms = []
                c0 = cpu_ms(name)
                t0 = time.perf_counter()
                for b in reqs:
                    t = time.perf_counter()
                    s, out = c.req("POST", "/%s/_search%s" % (IDX, qs), b)
                    ms.append((time.perf_counter() - t) * 1000)
                    if s != 200:
                        raise SystemExit("%s %s: %d %s" % (name, w, s, out[:300]))
                record(w, name, (time.perf_counter() - t0) * 1000 / a.reps, ms)
                record_cpu(w, name, c0, a.reps)
        if want("get"):
            for name, url in targets:
                c = Client(url)
                rng = random.Random(99 + rnd)
                ids = [rng.randrange(a.docs) for _ in range(a.reps * 2)]
                ms = []
                c0 = cpu_ms(name)
                t0 = time.perf_counter()
                for i in ids:
                    t = time.perf_counter()
                    c.req("GET", "/%s/_doc/%d" % (IDX, i))
                    ms.append((time.perf_counter() - t) * 1000)
                record("get", name, (time.perf_counter() - t0) * 1000 / len(ids), ms)
                record_cpu("get", name, c0, len(ids))
        print("round %d done" % rnd, file=sys.stderr, flush=True)
    for name, url in targets if not a.keep else []:
        Client(url).req("DELETE", "/" + IDX)

    result = {"docs": a.docs, "rounds": a.rounds, "reps": a.reps, "targets": [n for n, _ in targets], "workloads": {}}
    names = [n for n, _ in targets]
    print("| workload | " + " | ".join("%s median (min-max)" % n for n in names) + " | " + " | ".join("%s p50 / p99" % n for n in names) + " |")
    print("|---|" + "---:|" * (2 * len(names)))
    for w, per in rounds.items():
        row = {}
        cells, tails = [], []
        for n in names:
            xs = per.get(n, [])
            med = statistics.median(xs) if xs else 0
            unit = "s" if w.startswith("bulk") else "ms"
            scale = 1000 if unit == "s" else 1
            row[n] = {"median": med, "min": min(xs) if xs else 0, "max": max(xs) if xs else 0, "rounds": xs}
            cells.append("%.3f %s (%.3f-%.3f)" % (med / scale, unit, min(xs) / scale, max(xs) / scale) if xs else "-")
            ls = lat[w].get(n, [])
            if ls:
                row[n]["p50"], row[n]["p99"] = pct(ls, 50), pct(ls, 99)
                tails.append("%.3f / %.3f" % (row[n]["p50"], row[n]["p99"]))
            else:
                tails.append("-")
        result["workloads"][w] = row
        print("| %s | %s | %s |" % (w, " | ".join(cells), " | ".join(tails)))
    if cpu:
        print()
        print("Server CPU time per request (bulk: per load), median over rounds (min-max):")
        print()
        print("| workload | " + " | ".join("%s CPU" % n for n in names) + " |")
        print("|---|" + "---:|" * len(names))
        for w, per in cpu.items():
            cells = []
            for n in names:
                xs = per.get(n, [])
                if xs:
                    result["workloads"][w][n]["cpu_ms"] = statistics.median(xs)
                    unit, scale = ("s", 1000) if w.startswith("bulk") else ("ms", 1)
                    cells.append("%.3f %s (%.3f-%.3f)" % (statistics.median(xs) / scale, unit, min(xs) / scale, max(xs) / scale))
                else:
                    cells.append("-")
            print("| %s | %s |" % (w, " | ".join(cells)))
    if a.out:
        with open(a.out, "w") as f:
            json.dump(result, f, indent=1)


if __name__ == "__main__":
    main()
