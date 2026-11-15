#!/usr/bin/env python3
"""Captures Elasticsearch's answers to pipeline aggregations that run scripts, as fixtures.

usage: capture_pipeline.py <cases.txt> <out.json> [es_url]

Creates index `painless-pipe` (one shard, 30 daily documents with a double `v`, a long `n` and a
keyword `c`; some days have no document so histograms have empty buckets), then sends every case
(a JSON search body; cases separated by lines of `----`) to `POST /painless-pipe/_search` and
records the status and response. For a `bucket_selector` case the same request without the
selector is recorded too (`all`), so the harness can run the selector over every bucket. The
index is deleted afterwards.
"""
import copy, json, sys, time, urllib.request

INDEX = "painless-pipe"

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

def docs():
    out = []
    for day in range(30):
        if day % 7 == 3:
            continue  # an empty bucket every week
        out.append({"day": "2021-01-%02dT00:00:00Z" % (day + 1), "v": round((day * 37 % 11) * 1.25 - 3.5, 2), "n": day * 3 % 10, "c": "abc"[day % 3]})
    return out

def strip_selector(body):
    b = copy.deepcopy(body)
    def walk(aggs):
        for name in list(aggs.keys()):
            a = aggs[name]
            if "bucket_selector" in a:
                del aggs[name]
            elif "aggs" in a:
                walk(a["aggs"])
    walk(b.get("aggs", {}))
    return b

def main():
    cases_path, out_path = sys.argv[1], sys.argv[2]
    base = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    call(base, "DELETE", "/" + INDEX)
    mapping = {"properties": {"day": {"type": "date"}, "v": {"type": "double"}, "n": {"type": "long"}, "c": {"type": "keyword"}}}
    st, r = call(base, "PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0}, "mappings": mapping})
    assert st == 200, r
    for i, d in enumerate(docs()):
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
        entry = {"request": body, "status": status, "response": resp}
        if "bucket_selector" in text:
            _, all_resp = call(base, "POST", "/%s/_search" % INDEX, strip_selector(body))
            all_resp.pop("took", None)
            entry["all"] = all_resp
        res.append(entry)
    call(base, "DELETE", "/" + INDEX)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
