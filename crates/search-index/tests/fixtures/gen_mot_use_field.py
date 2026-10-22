#!/usr/bin/env python3
"""match_only_text intervals with `use_field`: queries on a `match_only_text` field `m` whose
sources partly read a `text` field `t` (indexed positions), as Elasticsearch 9.5.5 answers
them (hits, scores, explanations).

    python3 -I tests/fixtures/gen_mot_use_field.py http://127.0.0.1:19200 tests/fixtures/mot_use_field.json

Uses (and deletes) the index `index-fx-motuf`. The query list is mirrored in
`tests/source_confirmed.rs` (`use_field_cases`), in the same order.
"""
import json, random, sys, urllib.request

ES = sys.argv[1].rstrip("/")
OUT = sys.argv[2]
INDEX = "index-fx-motuf"
random.seed(7)


def req(method, path, body=None, nd=False):
    data = None if body is None else (body.encode() if nd else json.dumps(body).encode())
    ct = "application/x-ndjson" if nd else "application/json"
    r = urllib.request.Request(ES + path, data=data, method=method, headers={"Content-Type": ct})
    try:
        with urllib.request.urlopen(r) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


WORDS = ["quick", "brown", "fox", "lazy", "dog", "jumps", "over", "the", "red", "cat"]
DOCS = []
for i in range(60):
    m = " ".join(random.choice(WORDS) for _ in range(random.randint(2, 9)))
    t = " ".join(random.choice(WORDS) for _ in range(random.randint(2, 9)))
    d = {"m": m, "t": t}
    if i % 11 == 0:
        d = {"m": [m, "fox dog"], "t": t}
    if i % 13 == 0:
        d = {"t": t}
    DOCS.append(d)

M = lambda q, **kw: {"match": dict({"query": q}, **kw)}
QUERIES = [
    {"all_of": {"intervals": [M("quick"), M("fox", use_field="t")]}},
    {"all_of": {"ordered": True, "intervals": [M("brown"), M("dog", use_field="t")]}},
    {"any_of": {"intervals": [M("lazy cat", ordered=True), M("red fox", use_field="t", max_gaps=1)]}},
    M("quick fox", use_field="t"),
    M("the dog", use_field="t", ordered=True),
    {"all_of": {"intervals": [M("fox dog", max_gaps=2), M("jumps", use_field="t")]}},
    {"any_of": {"intervals": [{"prefix": {"prefix": "laz"}}, {"prefix": {"prefix": "re", "use_field": "t"}}]}},
    {"all_of": {"intervals": [M("cat"), {"wildcard": {"pattern": "j*s", "use_field": "t"}}], "filter": {"not_containing": M("over")}}},
]


def main():
    req("DELETE", "/" + INDEX)
    r = req("PUT", "/" + INDEX, {"settings": {"number_of_shards": 1}, "mappings": {"properties": {"m": {"type": "match_only_text"}, "t": {"type": "text"}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, d in enumerate(DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps(d))
    r = req("POST", f"/{INDEX}/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r.get("errors"), r
    req("POST", f"/{INDEX}/_forcemerge?max_num_segments=1")
    req("POST", f"/{INDEX}/_refresh")
    cases = []
    for q in QUERIES:
        body = {"query": {"intervals": {"m": q}}, "size": 100, "explain": True}
        r = req("POST", f"/{INDEX}/_search", body)
        assert "error" not in r, r
        hits = [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in r["hits"]["hits"]]
        cases.append({"query": q, "total": r["hits"]["total"]["value"], "hits": hits})
    req("DELETE", "/" + INDEX)
    json.dump({"docs": DOCS, "cases": cases}, open(OUT, "w"), indent=1)
    print("ok", [c["total"] for c in cases])


main()
