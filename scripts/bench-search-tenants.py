#!/usr/bin/env python3
"""Memory per tenant and wake time: roll search (rolld, one cluster per tenant, scale to zero)
against Elasticsearch / OpenSearch holding one index per tenant (they have no per-tenant unload).

    scripts/bench-search-tenants.py roll --api http://127.0.0.1:7071 --es http://127.0.0.1:59201 --pid <rolld pid> [--tenants 100] [--docs 200]
    scripts/bench-search-tenants.py index --url http://127.0.0.1:19210 --container <name> [--tenants 100] [--docs 200]

roll: RSS of rolld idle with no cluster, with N clusters created (asleep), with all N loaded
(each with an index of `--docs` small documents), and after they scaled to zero again; the wake
latency of a sleeping cluster's first search (p50 / p99 over the N clusters). For an index per
tenant: RSS of the container's JVM with no index and with N indices of the same documents.
Prints one JSON object.
"""

import argparse
import base64
import json
import statistics
import subprocess
import time
import urllib.request


def req(method, url, body=None, headers=None, timeout=120):
    data = None
    h = dict(headers or {})
    if body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        h.setdefault("Content-Type", "application/json")
    r = urllib.request.Request(url, data=data, method=method, headers=h)
    with urllib.request.urlopen(r, timeout=timeout) as resp:
        text = resp.read()
    return json.loads(text) if text else None


def rss_pid(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    return 0


def rss_container(name):
    """The largest VmRSS among the container's processes (its JVM)."""
    cid = subprocess.check_output(["docker", "inspect", "-f", "{{.Id}}", name], text=True).strip()
    for d in (f"/sys/fs/cgroup/system.slice/docker-{cid}.scope", f"/sys/fs/cgroup/docker/{cid}"):
        try:
            pids = open(f"{d}/cgroup.procs").read().split()
        except OSError:
            continue
        return max((rss_pid(p) for p in pids), default=0)
    return 0


def settle(read, seconds=8):
    """RSS after the allocator has had time to give memory back: the lowest of a few readings."""
    vals = []
    for _ in range(seconds):
        time.sleep(1)
        vals.append(read())
    return min(vals)


def bulk_body(index, docs):
    lines = []
    for i in range(docs):
        lines.append(json.dumps({"index": {"_index": index, "_id": str(i)}}))
        lines.append(json.dumps({"title": f"item {i} of {index}", "n": i, "tag": f"t{i % 7}"}))
    return ("\n".join(lines) + "\n").encode()


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def roll(a):
    mib = lambda b: round(b / 2**20, 1)
    out = {"target": "roll", "tenants": a.tenants, "docs": a.docs}
    out["rss_idle_empty_mib"] = mib(settle(lambda: rss_pid(a.pid)))
    creds = []
    for i in range(a.tenants):
        c = req("POST", f"{a.api}/v1/search/clusters", {"name": f"t{i:04d}", "idle_timeout_s": a.idle})
        creds.append((c["name"], c["password"]))
    out["rss_asleep_mib"] = mib(settle(lambda: rss_pid(a.pid)))
    auth = lambda n, p: {"Authorization": "Basic " + base64.b64encode(f"{n}:{p}".encode()).decode()}
    for n, p in creds:
        req("POST", f"{a.es}/_bulk?refresh=true", bulk_body("items", a.docs), {**auth(n, p), "Content-Type": "application/x-ndjson"})
        req("POST", f"{a.es}/items/_search", {"query": {"match": {"title": "item"}}}, auth(n, p))
    loaded = settle(lambda: rss_pid(a.pid))
    out["rss_all_loaded_mib"] = mib(loaded)
    out["per_loaded_tenant_kib"] = round((loaded - out["rss_asleep_mib"] * 2**20) / a.tenants / 1024, 1)
    # Wait for every cluster to scale to zero (idle timeout plus the reaper's period).
    deadline = time.time() + a.idle + 120
    while time.time() < deadline:
        info = req("GET", f"{a.api}/v1/search")
        if info.get("loaded", 0) == 0:
            break
        time.sleep(2)
    out["rss_after_scale_to_zero_mib"] = mib(settle(lambda: rss_pid(a.pid)))
    wakes = []
    for n, p in creds:
        t0 = time.perf_counter()
        req("POST", f"{a.es}/items/_search", {"query": {"match": {"title": "item"}}}, auth(n, p))
        wakes.append((time.perf_counter() - t0) * 1000)
    out["wake_ms_p50"] = round(statistics.median(wakes), 2)
    out["wake_ms_p99"] = round(pct(wakes, 99), 2)
    out["rss_after_waking_all_mib"] = mib(settle(lambda: rss_pid(a.pid)))
    for n, _ in creds:
        req("DELETE", f"{a.api}/v1/search/clusters/{n}")
    print(json.dumps(out))


def index(a):
    mib = lambda b: round(b / 2**20, 1)
    out = {"target": a.name, "tenants": a.tenants, "docs": a.docs}
    out["rss_idle_empty_mib"] = mib(settle(lambda: rss_container(a.container)))
    for i in range(a.tenants):
        idx = f"t{i:04d}"
        req("PUT", f"{a.url}/{idx}", {"settings": {"number_of_shards": 1, "number_of_replicas": 0}})
        req("POST", f"{a.url}/_bulk?refresh=true", bulk_body(idx, a.docs), {"Content-Type": "application/x-ndjson"})
        req("POST", f"{a.url}/{idx}/_search", {"query": {"match": {"title": "item"}}})
    loaded = settle(lambda: rss_container(a.container))
    out["rss_all_loaded_mib"] = mib(loaded)
    out["per_loaded_tenant_kib"] = round((loaded - out["rss_idle_empty_mib"] * 2**20) / a.tenants / 1024, 1)
    names = ",".join(f"t{i:04d}" for i in range(a.tenants))
    req("DELETE", f"{a.url}/{names}")
    print(json.dumps(out))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("roll")
    r.add_argument("--api", required=True)
    r.add_argument("--es", required=True)
    r.add_argument("--pid", type=int, required=True)
    r.add_argument("--idle", type=int, default=10, help="idle timeout of the clusters, seconds")
    x = sub.add_parser("index")
    x.add_argument("--url", required=True)
    x.add_argument("--container", required=True)
    x.add_argument("--name", default="es")
    for p in (r, x):
        p.add_argument("--tenants", type=int, default=100)
        p.add_argument("--docs", type=int, default=200)
    a = ap.parse_args()
    roll(a) if a.cmd == "roll" else index(a)


if __name__ == "__main__":
    main()
