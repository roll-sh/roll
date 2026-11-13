#!/usr/bin/env python3
"""Captures es-9.5.5-automata-random.json from a running Elasticsearch 9.5.5.

Indexes a deterministic pseudo-random set of keyword terms (ASCII, regexp/wildcard
metacharacters, multi-byte and case-folding characters) and records which of them
Elasticsearch matches for pseudo-random `fuzzy`, `regexp`, `wildcard` and `prefix` queries
(and the exception type and reason for invalid regexps). The codec's automata are checked
against the file in tests/es_fixtures.rs.

Usage: python3 -I gen_automata_random.py [http://127.0.0.1:19200] > es-9.5.5-automata-random.json
"""

import json
import random
import sys
import urllib.error
import urllib.request

ES = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:19200"
INDEX = "codec-automata-random"


def call(method, path, body=None, ndjson=False):
    data = None
    headers = {}
    if body is not None:
        data = body.encode() if ndjson else json.dumps(body).encode()
        headers["content-type"] = "application/x-ndjson" if ndjson else "application/json"
    req = urllib.request.Request(ES + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req) as r:
            return json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return json.loads(e.read() or b"{}")


rng = random.Random(20261021)
ALPHABET = list("aaaabbbcccd") + list("AB01.9*?\\- kKs") + ["é", "ß", "ẞ", "İ", "i", "日", "本", "😀", "Ω", "ω", "σ", "ς", "Σ", "\u212a", "\u017f"]


def rand_term(max_len=7):
    return "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, max_len)))


terms = sorted({rand_term() for _ in range(700)})
index = {t: i for i, t in enumerate(terms)}


def mutate(t):
    s = list(t)
    for _ in range(rng.randint(0, 2)):
        op = rng.randint(0, 3)
        if op == 0 and s:
            del s[rng.randrange(len(s))]
        elif op == 1:
            s.insert(rng.randint(0, len(s)), rng.choice(ALPHABET))
        elif op == 2 and s:
            s[rng.randrange(len(s))] = rng.choice(ALPHABET)
        elif op == 3 and len(s) > 1:
            i = rng.randrange(len(s) - 1)
            s[i], s[i + 1] = s[i + 1], s[i]
    return "".join(s)


def regex_atom(depth):
    r = rng.random()
    if depth > 2 or r < 0.35:
        return rng.choice(["a", "b", "c", "d", "A", "é", "日", "😀", ".", "\\.", "\\*", "\\d", "\\w", "\\s", "\\D", "\\W",
                           "ω", "Σ", "ß", "k", "s", "İ", "[^\\A]", "[a-a]", "[^ω]"])
    if r < 0.5:
        inner = "".join(rng.choice(["a", "b", "c", "é", "A", "0-9", "a-c", "\\-", "日", "\\d"]) for _ in range(rng.randint(1, 3)))
        return "[" + ("^" if rng.random() < 0.3 else "") + inner + "]"
    if r < 0.62:
        return "(" + regex(depth + 1) + ")"
    if r < 0.7:
        return "(" + regex(depth + 1) + "|" + regex(depth + 1) + ")"
    if r < 0.76:
        lo = rng.randint(0, 120)
        return "<%d-%d>" % (lo, lo + rng.randint(0, 200))
    if r < 0.82:
        return rng.choice(["@", "#", "~a", "~(ab)", "\"a.c\"", "~(" + regex(depth + 1) + ")"])
    if r < 0.88:
        return regex_atom(depth + 1) + "&" + regex_atom(depth + 1)
    return regex_atom(depth + 1)


def regex(depth=0):
    parts = []
    for _ in range(rng.randint(1, 3)):
        a = regex_atom(depth)
        q = rng.random()
        if q < 0.15:
            a += "*"
        elif q < 0.25:
            a += "+"
        elif q < 0.33:
            a += "?"
        elif q < 0.4:
            lo = rng.randint(0, 3)
            a += "{%d,%d}" % (lo, lo + rng.randint(0, 2)) if rng.random() < 0.7 else "{%d}" % lo
        elif q < 0.46:
            # Stacked quantifiers, which Elasticsearch collapses before parsing.
            a += "".join(rng.choice("+*?") for _ in range(rng.randint(2, 4)))
        parts.append(a)
    return "".join(parts)


def broken(p):
    # Occasionally damage a pattern to capture Lucene's syntax errors.
    if not p or rng.random() < 0.85:
        return p
    i = rng.randrange(len(p) + 1)
    return p[:i] + rng.choice(["(", ")", "[", "]", "{", "}", "<", ">", "\\", "\"", "{9,1}", "<5-x>"]) + p[i:]


def wildcard():
    return "".join(rng.choice(ALPHABET + ["*", "*", "?", "?", "\\*", "\\?", "\\\\", "\\A", "\\a"]) for _ in range(rng.randint(1, 6)))


call("DELETE", "/" + INDEX)
call("PUT", "/" + INDEX, {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
                          "mappings": {"properties": {"t": {"type": "keyword"}}}})
bulk = "".join(json.dumps({"index": {}}) + "\n" + json.dumps({"t": t}) + "\n" for t in terms)
r = call("POST", "/%s/_bulk?refresh=true" % INDEX, bulk, ndjson=True)
assert not r.get("errors"), r


def run(query):
    r = call("POST", "/%s/_search" % INDEX, {"size": 10000, "query": query, "_source": ["t"]})
    if "error" in r:
        e = r["error"]
        # The exception under the shard's query_shard_exception (Lucene's own exception).
        cause = (e.get("failed_shards") or [{}])[0].get("reason", e)
        cause = cause.get("caused_by", cause)
        return {"error_type": cause["type"], "error": cause["reason"]}
    return {"matches": sorted(index[h["_source"]["t"]] for h in r["hits"]["hits"])}


out = {"description": "Pseudo-random keyword terms and fuzzy/regexp/wildcard/prefix queries with the indices of the "
                      "matching terms (or the exception) from Elasticsearch 9.5.5; generated by gen_automata_random.py",
       "terms": terms, "fuzzy": [], "regexp": [], "wildcard": [], "prefix": []}
for _ in range(250):
    value = mutate(rng.choice(terms)) if rng.random() < 0.8 else rand_term()
    if not value:
        value = "a"
    case = {"value": value, "edits": rng.randint(0, 2), "prefix": rng.randint(0, 3), "transpositions": rng.random() < 0.6}
    case.update(run({"fuzzy": {"t": {"value": value, "fuzziness": case["edits"], "prefix_length": case["prefix"],
                                      "transpositions": case["transpositions"], "max_expansions": 10000}}}))
    out["fuzzy"].append(case)
for _ in range(300):
    pattern = broken(regex())
    ci = rng.random() < 0.2
    case = {"pattern": pattern, "case_insensitive": ci}
    case.update(run({"regexp": {"t": {"value": pattern, "case_insensitive": ci}}}))
    out["regexp"].append(case)
# Patterns around the determinization work limit (10000).
for n in list(range(4, 16)) + [20, 30, 40]:
    for p in ["[ac]*a[ac]{%d}" % n, "(a|b)*a(a|b){%d}" % n, ".*a.{%d}" % n, "(.*a){%d}" % n]:
        case = {"pattern": p, "case_insensitive": False}
        case.update(run({"regexp": {"t": {"value": p}}}))
        out["regexp"].append(case)
for _ in range(200):
    pattern = wildcard()
    ci = rng.random() < 0.3
    case = {"pattern": pattern, "case_insensitive": ci}
    case.update(run({"wildcard": {"t": {"value": pattern, "case_insensitive": ci}}}))
    out["wildcard"].append(case)
for _ in range(120):
    prefix = rng.choice(terms)[: rng.randint(0, 3)] if rng.random() < 0.7 else rand_term(3)
    ci = rng.random() < 0.5
    case = {"prefix": prefix, "case_insensitive": ci}
    case.update(run({"prefix": {"t": {"value": prefix, "case_insensitive": ci}}}))
    out["prefix"].append(case)
call("DELETE", "/" + INDEX)
# One case per line keeps diffs readable.
sys.stdout.write("{\n")
items = list(out.items())
for n, (k, v) in enumerate(items):
    sys.stdout.write(json.dumps(k) + ": ")
    if isinstance(v, list) and k != "terms":
        sys.stdout.write("[\n" + ",\n".join(json.dumps(c, ensure_ascii=False) for c in v) + "\n]")
    else:
        sys.stdout.write(json.dumps(v, ensure_ascii=False))
    sys.stdout.write(",\n" if n + 1 < len(items) else "\n")
sys.stdout.write("}\n")
