#!/usr/bin/env python3
"""match_only_text fixture: phrase, phrase-prefix and term queries on a `match_only_text`
field (positions confirmed from _source) as Elasticsearch 9.5.5 answers them.

    tests/fixtures/gen_mot.py http://127.0.0.1:19200 tests/fixtures/mot.json

Uses (and deletes) the index `index-fx-mot`.
"""
import json
import sys
import urllib.request

ES = sys.argv[1].rstrip("/")
OUT = sys.argv[2]
INDEX = "index-fx-mot"


def req(method, path, body=None, nd=False):
    data = None if body is None else (body.encode() if nd else json.dumps(body).encode())
    ct = "application/x-ndjson" if nd else "application/json"
    r = urllib.request.Request(ES + path, data=data, method=method, headers={"Content-Type": ct})
    try:
        with urllib.request.urlopen(r) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


DOCS = [
    "the quick brown fox jumps over the lazy dog",
    "quick quick brown brown fox",
    "brown quick fox",
    "a quick red fox and a brown dog",
    ["the lazy dog", "the quick brown cat"],
    ["over the lazy", "dog sleeps"],
    "Quick Brown Foxes are QUICK",
    "fox",
    "",
    ["brown", "quick"],
    "quickly browning the quickest brown fox",
    "the dog the dog the dog",
]
QUERIES = [
    {"match_phrase": {"m": "quick brown"}},
    {"match_phrase": {"m": {"query": "quick fox", "slop": 1}}},
    {"match_phrase": {"m": {"query": "brown quick", "slop": 2}}},
    {"match_phrase": {"m": {"query": "fox quick", "slop": 3}}},
    {"match_phrase": {"m": "the lazy dog"}},
    {"match_phrase": {"m": "lazy dog sleeps"}},
    {"match_phrase": {"m": {"query": "lazy dog", "slop": 101}}},
    {"match_phrase": {"m": {"query": "brown quick", "slop": 101}}},
    {"match_phrase": {"m": "the dog"}},
    {"match_phrase_prefix": {"m": "quick br"}},
    {"match_phrase_prefix": {"m": "qu"}},
    {"match_phrase_prefix": {"m": {"query": "the qu", "slop": 1}}},
    {"match": {"m": "fox"}},
    {"match": {"m": {"query": "quick fox", "operator": "and"}}},
    {"match": {"m": "brown dog"}},
    {"bool": {"must": [{"match_phrase": {"m": "quick brown"}}], "filter": [{"match": {"m": "fox"}}]}},
    {"match_phrase": {"m": {"query": "quick brown", "boost": 3}}},
]

req("DELETE", "/" + INDEX)
req("PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
                         "mappings": {"properties": {"m": {"type": "match_only_text"}}}})
bulk = ""
for i, v in enumerate(DOCS):
    bulk += json.dumps({"index": {"_id": str(i)}}) + "\n" + json.dumps({"m": v}) + "\n"
req("POST", f"/{INDEX}/_bulk?refresh=true", bulk, nd=True)
cases = []
for q in QUERIES:
    res = req("POST", f"/{INDEX}/_search", {"size": 100, "query": q, "explain": True})
    hits = [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in res["hits"]["hits"]]
    cases.append({"query": q, "total": res["hits"]["total"]["value"], "hits": hits})
req("DELETE", "/" + INDEX)

# Intervals (Elasticsearch's SourceIntervalsSource), on the YAML suite's corpus.
IV_DOCS = [None, "Apache Lucene powers Elasticsearch", "Elasticsearch is based on Apache Lucene",
           "The Apache Software Foundation manages many projects including Lucene",
           ["lucene apache", "apache lucene lucene"]]
IV_QUERIES = [
    {"intervals": {"m": {"match": {"query": "apache lucene", "max_gaps": 1}}}},
    {"intervals": {"m": {"match": {"query": "apache lucene", "ordered": True}}}},
    {"intervals": {"m": {"prefix": {"prefix": "luc"}}}},
    {"intervals": {"m": {"wildcard": {"pattern": "*ase*"}}}},
    {"intervals": {"m": {"fuzzy": {"term": "lucane"}}}},
    {"intervals": {"m": {"match": {"query": "lucene"}, "boost": 2}}},
]
req("PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
                         "mappings": {"properties": {"m": {"type": "match_only_text"}}}})
bulk = ""
for i, v in enumerate(IV_DOCS):
    bulk += json.dumps({"index": {"_id": str(i)}}) + "\n" + json.dumps({} if v is None else {"m": v}) + "\n"
req("POST", f"/{INDEX}/_bulk?refresh=true", bulk, nd=True)
iv_cases = []
for q in IV_QUERIES:
    res = req("POST", f"/{INDEX}/_search", {"size": 100, "query": q, "explain": True})
    if "hits" not in res:
        sys.exit(f"{q}: {res}")
    hits = [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in res["hits"]["hits"]]
    iv_cases.append({"query": q, "total": res["hits"]["total"]["value"], "hits": hits})
req("DELETE", "/" + INDEX)
json.dump({"docs": DOCS, "cases": cases, "iv_docs": IV_DOCS, "iv_cases": iv_cases}, open(OUT, "w"), indent=1)
print(len(cases), "cases")
