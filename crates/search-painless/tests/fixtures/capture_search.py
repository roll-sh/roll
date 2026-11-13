#!/usr/bin/env python3
"""Captures Elasticsearch's answers to search requests that run scripts, as fixtures.

usage: capture_search.py <cases.txt> <out.json> [es_url]

Creates index `painless-search` (one shard, the mapping in `painless-search.mapping.json`, the
documents in `painless-search.docs.json` with ids `1`..`n` in that order), then sends every case
(a JSON search body; cases separated by lines of `----`) to `POST /painless-search/_search` and
records the status and response. The index is deleted afterwards.
"""
import json, os, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
INDEX = "painless-search"

def call(base, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    while True:
        req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req) as r:
                status, resp = r.status, json.load(r)
        except urllib.error.HTTPError as e:
            status, resp = e.code, json.load(e)
        if "Too many dynamic script compilations" in json.dumps(resp):
            time.sleep(10)
            continue
        return status, resp

def main():
    cases_path, out_path = sys.argv[1], sys.argv[2]
    base = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    mapping = json.load(open(os.path.join(HERE, INDEX + ".mapping.json")))
    docs = json.load(open(os.path.join(HERE, INDEX + ".docs.json")))
    call(base, "DELETE", "/" + INDEX)
    st, r = call(base, "PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0}, "mappings": mapping})
    assert st == 200, r
    for i, d in enumerate(docs):
        call(base, "PUT", "/%s/_doc/%d" % (INDEX, i + 1), d)
    call(base, "POST", "/%s/_refresh" % INDEX)
    raw = open(cases_path, encoding="utf-8").read().split("\n----\n")
    res = []
    for c in raw:
        lines = [l for l in c.split("\n") if not l.startswith("#")]
        text = "\n".join(lines).strip()
        if not text:
            continue
        body = json.loads(text)
        status, resp = call(base, "POST", "/%s/_search" % INDEX, body)
        resp.pop("took", None)
        res.append({"request": body, "status": status, "response": resp})
    call(base, "DELETE", "/" + INDEX)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
