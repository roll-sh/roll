#!/usr/bin/env python3
"""Captures scripted-similarity scores from Elasticsearch as fixtures.

usage: capture_similarity.py <cases.json> <out.json> [es_url]

Every case ({"script", "weight_script"?, "query"?}) gets a fresh one-shard index
`painless-sim` whose text field uses a `scripted` similarity with those scripts, a fixed set of
documents, and a search with `explain`. The fixture records, per hit, the score and the inputs
of each term's similarity (`weight`, `query.boost`, `field.*`, `term.*`, `doc.*` as the
explanation shows them), or the error of a failed search.
"""
import json, sys, urllib.request

INDEX = "painless-sim"
DOCS = [
    "the quick brown fox",
    "quick quick fox jumps over the lazy dog",
    "lazy dog",
    "fox fox fox fox fox",
    "a quick movement of the enemy will jeopardize six gunboats quick quick",
]

def call(base, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.load(e)

def terms(expl):
    """The similarity inputs of every term clause of an explanation."""
    out = []
    def walk(e):
        if e["description"].startswith("score from ScriptedSimilarity"):
            out.append({d["description"]: d["value"] for d in e["details"]} | {"score": e["value"]})
            return
        for d in e.get("details", []):
            walk(d)
    walk(expl)
    return out

def main():
    cases_path, out_path = sys.argv[1], sys.argv[2]
    base = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    res = []
    for case in json.load(open(cases_path, encoding="utf-8")):
        sim = {"type": "scripted", "script": {"source": case["script"]}}
        if "weight_script" in case:
            sim["weight_script"] = {"source": case["weight_script"]}
        call(base, "DELETE", "/" + INDEX)
        status, resp = call(base, "PUT", "/" + INDEX, {
            "settings": {"number_of_shards": 1, "number_of_replicas": 0, "similarity": {"s": sim}},
            "mappings": {"properties": {"f": {"type": "text", "similarity": "s"}}},
        })
        entry = {"request": case}
        if status != 200:
            entry.update({"status": status, "response": resp})
            res.append(entry)
            continue
        for i, d in enumerate(DOCS):
            call(base, "PUT", "/%s/_doc/%d" % (INDEX, i), {"f": d})
        call(base, "POST", "/%s/_refresh" % INDEX)
        query = case.get("query", {"match": {"f": "quick fox"}})
        status, resp = call(base, "POST", "/%s/_search?explain=true" % INDEX, {"query": query, "size": 10})
        entry["status"] = status
        if status == 200 and not resp.get("_shards", {}).get("failures"):
            entry["hits"] = [{"id": h["_id"], "score": h["_score"], "terms": terms(h["_explanation"])} for h in resp["hits"]["hits"]]
        else:
            entry["response"] = resp
        res.append(entry)
    call(base, "DELETE", "/" + INDEX)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
