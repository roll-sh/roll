#!/usr/bin/env python3
"""Captures `significant_terms` scores with a `script_heuristic` from Elasticsearch as fixtures.

usage: capture_heuristic.py <cases.json> <out.json> [es_url]

A one-shard index `painless-heur` holds fixed documents (`cat`, `tag` keywords); every case
({"script", "params"?}) runs a `significant_terms` aggregation on `tag` over the documents of
`cat: a` with the script as heuristic, and records the response (bucket scores and counts, or
the error).
"""
import json, sys, time, urllib.request

INDEX = "painless-heur"

def call(base, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.load(e)

def main():
    cases_path, out_path = sys.argv[1], sys.argv[2]
    base = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    call(base, "DELETE", "/" + INDEX)
    call(base, "PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
                                     "mappings": {"properties": {"cat": {"type": "keyword"}, "tag": {"type": "keyword"}}}})
    docs = []
    for i in range(60):
        cat = "a" if i % 3 == 0 else ("b" if i % 3 == 1 else "c")
        tags = ["t%d" % (i % 7)]
        if cat == "a":
            tags.append("hot")
        if i % 5 == 0:
            tags.append("five")
        if i % 11 == 0:
            tags.append("rare")
        docs.append({"cat": cat, "tag": tags})
    for i, d in enumerate(docs):
        call(base, "PUT", "/%s/_doc/%d" % (INDEX, i), d)
    call(base, "POST", "/%s/_refresh" % INDEX)
    res = []
    # every candidate term with its counts: a heuristic scoring them all 1
    baseline = None
    # a fresh source per run: Painless call sites keep what earlier executions linked
    nonce = "/* %d */ " % int(time.time())
    for case in [{"script": "1", "baseline": True}] + json.load(open(cases_path, encoding="utf-8")):
        case = dict(case, script=nonce + case["script"])
        script = {"source": case["script"]}
        if "params" in case:
            script["params"] = case["params"]
        body = {"size": 0, "query": {"term": {"cat": "a"}},
                "aggs": {"s": {"significant_terms": {"field": "tag", "min_doc_count": 1, "size": 20, "script_heuristic": {"script": script}}}}}
        status, resp = call(base, "POST", "/%s/_search" % INDEX, body)
        resp.pop("took", None)
        if case.get("baseline"):
            baseline = resp["aggregations"]["s"]
            continue
        res.append({"request": case, "status": status, "response": resp, "candidates": baseline})
    call(base, "DELETE", "/" + INDEX)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
