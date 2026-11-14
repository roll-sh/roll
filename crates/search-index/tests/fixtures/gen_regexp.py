#!/usr/bin/env python3
"""Regexp fixture: keyword values and Lucene regexps (flags, case_insensitive, errors) as
Elasticsearch 9.5.5 evaluates them.

    tests/fixtures/gen_regexp.py http://127.0.0.1:19200 tests/fixtures/regexp.json

Uses (and deletes) the index `index-fx-regexp`.
"""
import json
import random
import sys
import urllib.request

ES = sys.argv[1].rstrip("/")
OUT = sys.argv[2]
INDEX = "index-fx-regexp"


def req(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    r = urllib.request.Request(ES + path, data=data, method=method, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


rng = random.Random(7)
alphabet = "abcAB01.-~@&#<>"
values = sorted({"".join(rng.choice("abcAB019") for _ in range(rng.randrange(0, 7))) for _ in range(400)})
values += ["", "a.b", "a-b", "~a", "@", "a&b", "#", "<5>", "12", "7", "42", "100", "abc", "ABC", "aBc", "ééa", "日本"]
values = list(dict.fromkeys(values))

PATTERNS = [
    "a.*", "a+", "a*b?", "[ab]+", "[^a]*", "[a-c]{2,3}", "(ab|ba)c?", "a{2}", "a{2,}", "a{,2}", ".{3}",
    "<1-50>", "<05-10>", "<0-9>.*", "a~", "~a", "~(a.*)", "a&b", "(a.*)&(.*b)", "@", "a@", "#", "a#?",
    "\"a\"", "a\\.b", "a\\-b", "\\~a", "[a\\-b]+", "[]", "(", "a{3,1}", "[z-a]", "a\\", "<10-1>",
    "abc", "ABC", "aBc", "[[:alpha:]]+", "\\d+", "\\w+", "\\s*", "[\\d]+", "\\D+", "\\W*", "a|", "|a",
    "()", "a()b", ".", "..*", "日.", "é+a", "[é]+a", "1|7", "(1|7)+", "a**", "a+?", "a{1}{2}",
]
FLAGS = ["ALL", "NONE", "INTERSECTION", "COMPLEMENT", "EMPTY", "ANYSTRING", "INTERVAL", "INTERSECTION|ANYSTRING"]

req("DELETE", "/" + INDEX)
req("PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
                         "mappings": {"properties": {"k": {"type": "keyword"}}}})
bulk = ""
for i, v in enumerate(values):
    bulk += json.dumps({"index": {"_id": str(i)}}) + "\n" + json.dumps({"k": v}) + "\n"
r = urllib.request.Request(ES + f"/{INDEX}/_bulk?refresh=true", data=bulk.encode(), method="POST",
                           headers={"Content-Type": "application/x-ndjson"})
urllib.request.urlopen(r).read()

cases = []
for p in PATTERNS:
    for flags in (FLAGS if p in ("a~", "~a", "~(a.*)", "a&b", "(a.*)&(.*b)", "@", "a@", "#", "a#?", "<1-50>", "<05-10>", "a.*") else ["ALL"]):
        for ci in ([False, True] if any(c.isalpha() for c in p) and flags == "ALL" else [False]):
            q = {"regexp": {"k": {"value": p, "flags": flags, "case_insensitive": ci}}}
            res = req("POST", f"/{INDEX}/_search", {"size": 1000, "query": q, "_source": False})
            case = {"pattern": p, "flags": flags, "case_insensitive": ci}
            if "error" in res:
                e = res["error"]
                root = e.get("root_cause", [{}])[0]
                cause = e.get("caused_by") or {}
                while cause.get("caused_by"):
                    cause = cause["caused_by"]
                fs = (e.get("failed_shards") or [{}])[0].get("reason", {})
                inner = fs.get("caused_by") or fs
                case["error"] = {"type": inner.get("type", root.get("type")), "reason": inner.get("reason", root.get("reason"))}
            else:
                case["ids"] = sorted(int(h["_id"]) for h in res["hits"]["hits"])
            cases.append(case)

req("DELETE", "/" + INDEX)
json.dump({"values": values, "cases": cases}, open(OUT, "w"), ensure_ascii=False, indent=0)
print(len(cases), "cases")
