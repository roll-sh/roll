"""Client side of the roll search image smoke test (scripts/search-image-smoke.sh).

Talks to a search-proxy the way users do: the control API (/v1/search, bearer token) on one
port, Elasticsearch REST (Basic auth per cluster) on the other. Standard library only, so it
runs on any host with Python 3.9+.

  smoke.py create   <api> <token> <name>...            -> JSON {name: password} on stdout
  smoke.py es       <rest> <clusters.json>             Elasticsearch requests, checked
  smoke.py state    <api> <token> <name>               -> the cluster's state (loaded/asleep)
  smoke.py writer   <rest> <clusters.json> <stop file> <acked.json>
                    writes documents until the stop file exists; records every acknowledged id
  smoke.py verify   <rest> <clusters.json> <acked.json> [expected docs per cluster]
  smoke.py count    <rest> <clusters.json> <name> [timeout s]   -> one _count, held through a wake
"""

import base64
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request

INDEX = "smoke"


def call(url, method="GET", body=None, auth=None, timeout=30, ndjson=False):
    data = None
    headers = {}
    if body is not None:
        data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
        headers["content-type"] = "application/x-ndjson" if ndjson else "application/json"
    if auth:
        headers["authorization"] = auth
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw.decode(errors="replace")


def basic(name, password):
    return "Basic " + base64.b64encode(f"{name}:{password}".encode()).decode()


def check(cond, what):
    if not cond:
        raise SystemExit(f"smoke: FAILED: {what}")


def create(api, token, names):
    out = {}
    for name in names:
        # Retried while the proxy has no node yet (it holds, then answers 503 past its timeout).
        for attempt in range(60):
            status, doc = call(f"{api}/v1/search/clusters", "POST", {"name": name}, f"Bearer {token}", timeout=150)
            if status == 200:
                break
            time.sleep(1)
        check(status == 200 and doc.get("password"), f"create cluster {name}: {status} {doc}")
        check(doc["name"] == name and doc["username"] == name, f"create cluster {name}: {doc}")
        out[name] = doc["password"]
    status, doc = call(f"{api}/v1/search/clusters", auth=f"Bearer {token}")
    check(status == 200 and {c["name"] for c in doc} >= set(names), f"list clusters: {status} {doc}")
    status, doc = call(f"{api}/v1/search/clusters", auth="Bearer wrong")
    check(status == 401, f"a wrong API token must get 401, got {status}")
    print(json.dumps(out))


def es(rest, clusters):
    for name, password in clusters.items():
        a = basic(name, password)
        status, doc = call(f"{rest}/", auth=a)
        check(status == 200 and doc["version"]["number"] == "9.5.5" and doc["tagline"] == "You Know, for Search", f"GET / on {name}: {status} {doc}")
        check(doc["cluster_name"] == name, f"cluster_name of {name}: {doc}")
        status, doc = call(f"{rest}/", auth=basic(name, "wrong"))
        check(status == 401, f"a wrong password must get 401, got {status} {doc}")
        status, doc = call(f"{rest}/{INDEX}", "PUT", {"mappings": {"properties": {"title": {"type": "text"}, "tag": {"type": "keyword"}, "n": {"type": "long"}}}}, a)
        check(status == 200 and doc == {"acknowledged": True, "shards_acknowledged": True, "index": INDEX}, f"create index on {name}: {status} {doc}")
        lines = []
        for i in range(200):
            lines.append(json.dumps({"index": {"_id": f"b{i}"}}))
            lines.append(json.dumps({"title": f"quick brown fox {i}", "tag": f"t{i % 4}", "n": i}))
        status, doc = call(f"{rest}/{INDEX}/_bulk?refresh=wait_for", "POST", "\n".join(lines) + "\n", a, ndjson=True)
        check(status == 200 and doc["errors"] is False and len(doc["items"]) == 200, f"bulk on {name}: {status}")
        status, doc = call(f"{rest}/{INDEX}/_doc/b7", auth=a)
        check(status == 200 and doc["found"] and doc["_source"]["n"] == 7, f"get on {name}: {status} {doc}")
        query = {"query": {"match": {"title": "fox"}}, "aggs": {"tags": {"terms": {"field": "tag"}}, "total": {"sum": {"field": "n"}}}, "size": 3, "sort": [{"n": "desc"}]}
        status, doc = call(f"{rest}/{INDEX}/_search", "POST", query, a)
        check(status == 200 and doc["hits"]["total"] == {"value": 200, "relation": "eq"}, f"search on {name}: {status} {doc}")
        check([h["_id"] for h in doc["hits"]["hits"]] == ["b199", "b198", "b197"], f"sorted hits on {name}: {doc['hits']['hits']}")
        check(sorted((b["key"], b["doc_count"]) for b in doc["aggregations"]["tags"]["buckets"]) == [(f"t{i}", 50) for i in range(4)], f"terms on {name}: {doc['aggregations']}")
        check(doc["aggregations"]["total"]["value"] == sum(range(200)), f"sum on {name}: {doc['aggregations']}")
        status, doc = call(f"{rest}/{INDEX}/_count", auth=a)
        check(status == 200 and doc["count"] == 200, f"count on {name}: {status} {doc}")
        status, doc = call(f"{rest}/missing/_search", auth=a)
        check(status == 404 and doc["error"]["type"] == "index_not_found_exception", f"missing index on {name}: {status} {doc}")
        status, doc = call(f"{rest}/_cat/indices?format=json", auth=a)
        check(status == 200 and [i["index"] for i in doc] == [INDEX], f"_cat/indices on {name}: {status} {doc}")
    print(f"smoke: Elasticsearch requests on {len(clusters)} clusters: ok")


def state(api, token, name):
    status, doc = call(f"{api}/v1/search/clusters/{name}", auth=f"Bearer {token}")
    check(status == 200, f"show {name}: {status} {doc}")
    print(doc["status"]["state"])


def writer(rest, clusters, stop_file, acked_file):
    acked = {name: [] for name in clusters}
    errors = []
    lock = threading.Lock()

    def run(worker):
        names = sorted(clusters)
        i = 0
        while not os.path.exists(stop_file):
            name = names[(worker + i) % len(names)]
            doc_id = f"w{worker}-{i}"
            i += 1
            try:
                status, doc = call(f"{rest}/{INDEX}/_doc/{doc_id}", "PUT", {"title": "written during a drain", "n": i}, basic(name, clusters[name]), timeout=150)
            except Exception as e:  # a connection the proxy dropped: not acknowledged
                status, doc = 0, str(e)
            with lock:
                if status in (200, 201):
                    acked[name].append(doc_id)
                else:
                    errors.append((name, doc_id, status, str(doc)[:200]))

    threads = [threading.Thread(target=run, args=(w,)) for w in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    with open(acked_file, "w") as f:
        json.dump(acked, f)
    n = sum(len(v) for v in acked.values())
    print(f"smoke: writer: {n} writes acknowledged, {len(errors)} not")
    for e in errors[:5]:
        print(f"smoke: writer: not acknowledged: {e}")
    check(n > 0, "the writer got no write acknowledged")


def verify(rest, clusters, acked_file, base_docs):
    with open(acked_file) as f:
        acked = json.load(f)
    total = 0
    for name, ids in acked.items():
        a = basic(name, clusters[name])
        status, doc = call(f"{rest}/{INDEX}/_refresh", "POST", None, a, timeout=150)
        check(status == 200, f"refresh on {name}: {status} {doc}")
        for start in range(0, len(ids), 500):
            chunk = ids[start:start + 500]
            status, doc = call(f"{rest}/{INDEX}/_mget", "POST", {"ids": chunk}, a, timeout=150)
            check(status == 200, f"mget on {name}: {status} {doc}")
            missing = [d["_id"] for d in doc["docs"] if not d.get("found")]
            check(not missing, f"{len(missing)} acknowledged writes lost on {name}: {missing[:5]}")
        status, doc = call(f"{rest}/{INDEX}/_count", auth=a, timeout=150)
        check(status == 200 and doc["count"] >= base_docs + len(ids), f"count on {name}: {status} {doc}, want >= {base_docs + len(ids)}")
        total += len(ids)
    print(f"smoke: all {total} acknowledged writes are there")


def count(rest, clusters, name, timeout):
    t = time.monotonic()
    status, doc = call(f"{rest}/{INDEX}/_count", auth=basic(name, clusters[name]), timeout=timeout)
    check(status == 200, f"count on {name}: {status} {doc}")
    print(json.dumps({"count": doc["count"], "seconds": round(time.monotonic() - t, 2)}))


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    load = lambda p: json.load(open(p))  # noqa: E731
    if cmd == "create":
        create(args[0], args[1], args[2:])
    elif cmd == "es":
        es(args[0], load(args[1]))
    elif cmd == "state":
        state(*args)
    elif cmd == "writer":
        writer(args[0], load(args[1]), args[2], args[3])
    elif cmd == "verify":
        verify(args[0], load(args[1]), args[2], int(args[3]) if len(args) > 3 else 0)
    elif cmd == "count":
        count(args[0], load(args[1]), args[2], float(args[3]) if len(args) > 3 else 150)
    else:
        raise SystemExit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
