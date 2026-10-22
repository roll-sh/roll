#!/usr/bin/env python3
"""Captures Elasticsearch's answers to `lang: expression` scripts (Lucene expressions) as fixtures.

usage: capture_expression.py <cases.txt> <out.json> [es_url]

Index `painless-expr` (one shard, mapping `painless-expr.mapping.json`) holds the documents of
`painless-expr.docs.json` (ids "1", "2", …). Cases are separated by lines of `----`; header lines
`#context: field|score|sort|aggs|value|filter` (default `field`) and `#params: {json}` (a case
holding only `#defaults: {...}` sets defaults). Each case runs one search:

  field   script_fields, every document in `_doc` order       -> hits[].fields.v
  score   script_score over match_all, `_doc` order            -> hits[]._score
  sort    a `number` script sort                                -> hits[]._id and sort values
  aggs    min and max aggregations of the script                -> aggregations
  value   a sum aggregation over field `l` with the script (`_value`) -> aggregations
  filter  a script query, `_doc` order                          -> hits[]._id

The response is recorded as is; `tests/expression.rs` replays each case against the same
documents.
"""
import json, os, sys, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
INDEX = "painless-expr"

def req(url, method, body=None):
    r = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), method=method, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(r) as resp:
            return resp.status, json.load(resp)
    except urllib.error.HTTPError as e:
        return e.code, json.load(e)

def parse(path):
    raw, cur = [], []
    for line in open(path, encoding="utf-8").read().split("\n"):
        if line == "----":
            raw.append("\n".join(cur)); cur = []
        else:
            cur.append(line)
    if any(l.strip() for l in cur):
        raw.append("\n".join(cur))
    out, defaults = [], {}
    for c in raw:
        lines = c.split("\n")
        hdr = {}
        while lines and lines[0].startswith("#") and lines[0].split(":")[0] in ("#context", "#params", "#defaults"):
            k, v = lines[0][1:].split(":", 1)
            hdr[k] = v.strip()
            lines = lines[1:]
        src = "\n".join(lines).strip("\n")
        if "defaults" in hdr:
            defaults = json.loads(hdr.pop("defaults"))
        if not src.strip():
            continue
        h = dict(defaults)
        h.update({k: (json.loads(v) if k == "params" else v) for k, v in hdr.items()})
        out.append((h, src))
    return out

def body(ctx, script):
    if ctx == "field":
        return {"sort": ["_doc"], "script_fields": {"v": {"script": script}}}
    if ctx == "score":
        return {"sort": ["_doc"], "track_scores": True, "query": {"script_score": {"query": {"match_all": {}}, "script": script}}}
    if ctx == "sort":
        return {"sort": [{"_script": {"type": "number", "script": script, "order": "asc"}}, "_doc"]}
    if ctx == "aggs":
        return {"size": 0, "aggs": {"mn": {"min": {"script": script}}, "mx": {"max": {"script": script}}}}
    if ctx == "value":
        return {"size": 0, "aggs": {"s": {"sum": {"field": "l", "script": script}}}}
    if ctx == "filter":
        return {"sort": ["_doc"], "query": {"script": {"script": script}}}
    raise SystemExit(f"unknown context {ctx}")

def main():
    es = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    mapping = json.load(open(os.path.join(HERE, INDEX + ".mapping.json")))
    docs = json.load(open(os.path.join(HERE, INDEX + ".docs.json")))
    req(f"{es}/{INDEX}", "DELETE")
    st, r = req(f"{es}/{INDEX}", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 0}, "mappings": mapping})
    assert st == 200, r
    for i, d in enumerate(docs):
        req(f"{es}/{INDEX}/_doc/{i + 1}", "PUT", d)
    req(f"{es}/{INDEX}/_refresh", "POST")
    res = []
    for hdr, src in parse(sys.argv[1]):
        ctx = hdr.get("context", "field")
        script = {"lang": "expression", "source": src}
        if "params" in hdr:
            script["params"] = hdr["params"]
        status, resp = req(f"{es}/{INDEX}/_search", "POST", body(ctx, script))
        for h in resp.get("hits", {}).get("hits", []):
            h.pop("_index", None); h.pop("_ignored", None)
        res.append({"context": ctx, "script": script, "status": status, "response": resp})
    req(f"{es}/{INDEX}", "DELETE")
    with open(sys.argv[2], "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
