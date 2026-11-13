"""Captures Elasticsearch 9.5.5 behaviour the engine must reproduce, as JSON fixtures.

Usage: python3 -I gen_es_fixtures.py http://127.0.0.1:19200 <out dir> [fvh|ivgraph|completion|features|highlight_phrases]
(`fvh` / `ivgraph` capture only that fixture.)

Creates indices prefixed `index-fx-` (deleted afterwards), indexes a small corpus (one bulk,
one refresh: one segment, doc ids = insertion order), and records:
- search.json: queries with every hit's score and explanation (`explain: true`);
- highlight.json: highlighting results for texts, queries and options;
- highlight_phrases.json: unified and plain highlighting of (sloppy) phrases next to terms;
- fvh.json: the same for the fast vector highlighter (term vectors with offsets);
- ivgraph.json: intervals `match` rules analyzed with a synonym_graph analyzer (token
  graphs): Lucene's query string (`_validate/query?explain`), hits and scores;
- features.json: rank_feature / sparse_vector (Lucene feature queries), combined_fields
  (BM25F), match_phrase_prefix, pinned, distance_feature (dates, geo points) and
  has_child / has_parent scores, explanations and query strings, `_geo_distance` and
  `_script` sorts, term and phrase suggestions, and scroll / point-in-time slices;
- completion.json: completion suggestions (prefix, fuzzy, skip_duplicates) over three
  segments with a deleted doc.
The Rust side rebuilds each query from its `q` description (tests/es_fixtures.rs).
"""
import json, sys, urllib.request

ES = sys.argv[1].rstrip('/')
OUT = sys.argv[2]

def req(method, path, body=None, nd=False):
    data = None
    headers = {}
    if body is not None:
        data = (body if nd else json.dumps(body)).encode()
        headers['content-type'] = 'application/x-ndjson' if nd else 'application/json'
    r = urllib.request.Request(ES + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            return json.loads(resp.read() or b'null')
    except urllib.error.HTTPError as e:
        return json.loads(e.read())

DOCS = [
    "the quick brown fox jumps over the lazy dog",
    "quick quick fox",
    "lazy",
    "the fox and the hound are friends of the quick brown dog",
    "brown dogs and brown foxes jump quickly over lazy cats",
    "a quick movement of the enemy will jeopardize six gunboats",
    "fox fox fox fox fox",
    "the the the quick",
    "dog brown quick fox lazy",
    "over the hills and far away the quick fox ran",
]
NUMS = [5, 12, 7, 30, 1, 18, 25, 3, 9, 14]
TAGS = [["a"], ["b"], ["a", "c"], [], ["c"], ["b", "d"], ["a"], ["d"], ["e"], ["a", "b"]]
GROUPS = ["x", "y", "x", "z", "y", "x", None, "z", "y", "x"]
MINS = [1, 2, 1, 0, 1, 2, None, 1, 3, 2]

SIMS = {
    "bm25": None,
    "bm25b": {"type": "BM25", "k1": 1.6, "b": 0.3},
    "dfr": {"type": "DFR", "basic_model": "g", "after_effect": "l", "normalization": "h2", "normalization.h2.c": "3.0"},
    "dfr2": {"type": "DFR", "basic_model": "if", "after_effect": "b", "normalization": "h1"},
    "dfr3": {"type": "DFR", "basic_model": "in", "after_effect": "l", "normalization": "z", "normalization.z.z": "0.3"},
    "dfr4": {"type": "DFR", "basic_model": "ine", "after_effect": "b", "normalization": "h3", "normalization.h3.c": "900"},
    "ib": {"type": "IB", "distribution": "ll", "lambda": "df", "normalization": "z", "normalization.z.z": "0.25"},
    "ib2": {"type": "IB", "distribution": "spl", "lambda": "ttf", "normalization": "h2"},
    "dfi": {"type": "DFI", "independence_measure": "chisquared"},
    "dfi2": {"type": "DFI", "independence_measure": "standardized"},
    "lmd": {"type": "LMDirichlet", "mu": 3},
    "lmj": {"type": "LMJelinekMercer", "lambda": 0.4},
    "script": {"type": "scripted", "script": {"source": "double tf = Math.sqrt(doc.freq); double idf = Math.log((field.docCount+1.0)/(term.docFreq+1.0)) + 1.0; double norm = 1/Math.sqrt(doc.length); return query.boost * tf * idf * norm;"}},
    "script2": {"type": "scripted", "weight_script": {"source": "double idf = Math.log((field.docCount+1.0)/(term.docFreq+1.0)) + 1.0; return query.boost * idf;"},
                "script": {"source": "double tf = Math.sqrt(doc.freq); double norm = 1/Math.sqrt(doc.length); return weight * tf * norm;"}},
}

def setup():
    sims = {k: v for k, v in SIMS.items() if v}
    props = {"t": {"type": "text"}, "k": {"type": "keyword"}, "n": {"type": "long"}, "g": {"type": "keyword"}, "m": {"type": "long"}}
    for k in sims:
        props["t_" + k] = {"type": "text", "similarity": k}
    props["t_boolean"] = {"type": "text", "similarity": "boolean"}
    req("DELETE", "/index-fx-main")
    r = req("PUT", "/index-fx-main", {"settings": {"number_of_shards": 1, "similarity": sims}, "mappings": {"properties": props}})
    assert r.get("acknowledged"), r
    lines = []
    for i, text in enumerate(DOCS):
        doc = {"t": text, "k": TAGS[i], "n": NUMS[i]}
        if GROUPS[i] is not None:
            doc["g"] = GROUPS[i]
        if MINS[i] is not None:
            doc["m"] = MINS[i]
        for k in SIMS:
            if k != "bm25":
                doc["t_" + k] = text
        doc["t_boolean"] = text
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps(doc))
    r = req("POST", "/index-fx-main/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-main/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-main/_refresh")

# Query descriptions: `q` is what the Rust test rebuilds, `es` the Query DSL.
def term(f, v): return {"q": {"term": [f, v]}, "es": {"term": {f: v}}}
CASES = []
for f in ["t", "t_boolean"] + ["t_" + k for k in SIMS if k != "bm25"]:
    CASES.append({"name": "match_or_" + f, "q": {"or": [{"term": [f, "quick"]}, {"term": [f, "fox"]}, {"term": [f, "the"]}]}, "es": {"match": {f: "quick fox the"}}})
    CASES.append({"name": "phrase_" + f, "q": {"phrase": [f, ["quick", "brown"], 0]}, "es": {"match_phrase": {f: "quick brown"}}})
CASES += [
    {"name": "term_boost", "q": {"boost": [{"term": ["t", "fox"]}, 3.0]}, "es": {"term": {"t": {"value": "fox", "boost": 3.0}}}},
    {"name": "and", "q": {"and": [{"term": ["t", "quick"]}, {"term": ["t", "fox"]}]}, "es": {"match": {"t": {"query": "quick fox", "operator": "and"}}}},
    {"name": "msm", "q": {"msm": [[{"term": ["t", "quick"]}, {"term": ["t", "fox"]}, {"term": ["t", "dog"]}], 2]}, "es": {"match": {"t": {"query": "quick fox dog", "minimum_should_match": 2}}}},
    {"name": "sloppy", "q": {"phrase": ["t", ["quick", "fox"], 2]}, "es": {"match_phrase": {"t": {"query": "quick fox", "slop": 2}}}},
    {"name": "sloppy_rpt", "q": {"phrase": ["t", ["fox", "fox"], 1]}, "es": {"match_phrase": {"t": {"query": "fox fox", "slop": 1}}}},
    {"name": "phrase3", "q": {"phrase": ["t", ["the", "quick", "brown"], 0]}, "es": {"match_phrase": {"t": "the quick brown"}}},
    {"name": "filter_must_not", "q": {"bool": {"must": [{"term": ["t", "fox"]}], "filter": [{"term": ["t", "quick"]}], "must_not": [{"term": ["t", "lazy"]}]}}, "es": {"bool": {"must": [{"term": {"t": "fox"}}], "filter": [{"term": {"t": "quick"}}], "must_not": [{"term": {"t": "lazy"}}]}}},
    {"name": "should_filter", "q": {"bool": {"should": [{"term": ["t", "fox"]}, {"term": ["t", "dog"]}], "filter": [{"term": ["k", "a"]}]}}, "es": {"bool": {"should": [{"term": {"t": "fox"}}, {"term": {"t": "dog"}}], "filter": [{"term": {"k": "a"}}], "minimum_should_match": 1}}},
    {"name": "const", "q": {"const": [{"term": ["t", "fox"]}, 1.5]}, "es": {"constant_score": {"filter": {"term": {"t": "fox"}}, "boost": 1.5}}},
    {"name": "dismax", "q": {"dismax": [[{"term": ["t", "fox"]}, {"term": ["t", "quick"]}], 0.3]}, "es": {"dis_max": {"queries": [{"term": {"t": "fox"}}, {"term": {"t": "quick"}}], "tie_breaker": 0.3}}},
    {"name": "dismax0", "q": {"dismax": [[{"term": ["t", "fox"]}, {"term": ["t", "dog"]}], 0.0]}, "es": {"dis_max": {"queries": [{"term": {"t": "fox"}}, {"term": {"t": "dog"}}]}}},
    {"name": "prefix", "q": {"prefix": ["t", "qu"]}, "es": {"prefix": {"t": "qu"}}},
    {"name": "wildcard", "q": {"wildcard": ["t", "f*x"]}, "es": {"wildcard": {"t": "f*x"}}},
    {"name": "fuzzy", "q": {"fuzzy": ["t", "quack", 2]}, "es": {"fuzzy": {"t": {"value": "quack", "fuzziness": 2}}}},
    {"name": "fuzzy_auto", "q": {"fuzzy": ["t", "foxs", 1]}, "es": {"fuzzy": {"t": {"value": "foxs", "fuzziness": 1}}}},
    {"name": "range", "q": {"range": ["n", 5, 20]}, "es": {"range": {"n": {"gte": 5, "lte": 20}}}},
    {"name": "terms_kw", "q": {"set": ["k", ["a", "d"]]}, "es": {"terms": {"k": ["a", "d"]}}},
    {"name": "exists_kw", "q": {"exists": "k"}, "es": {"exists": {"field": "k"}}},
    {"name": "span_near", "q": {"span": {"near": [[{"t": "quick"}, {"t": "fox"}], 1, True]}}, "es": {"span_near": {"clauses": [{"span_term": {"t": "quick"}}, {"span_term": {"t": "fox"}}], "slop": 1, "in_order": True}}},
    {"name": "span_near_unordered", "q": {"span": {"near": [[{"t": "fox"}, {"t": "quick"}], 2, False]}}, "es": {"span_near": {"clauses": [{"span_term": {"t": "fox"}}, {"span_term": {"t": "quick"}}], "slop": 2, "in_order": False}}},
    {"name": "span_or", "q": {"span": {"or": [{"t": "lazy"}, {"t": "hound"}]}}, "es": {"span_or": {"clauses": [{"span_term": {"t": "lazy"}}, {"span_term": {"t": "hound"}}]}}},
    {"name": "span_first", "q": {"span": {"first": [{"t": "quick"}, 2]}}, "es": {"span_first": {"match": {"span_term": {"t": "quick"}}, "end": 2}}},
    {"name": "span_not", "q": {"span": {"not": [{"t": "quick"}, {"near": [[{"t": "quick"}, {"t": "fox"}], 0, True]}, 0, 0]}}, "es": {"span_not": {"include": {"span_term": {"t": "quick"}}, "exclude": {"span_near": {"clauses": [{"span_term": {"t": "quick"}}, {"span_term": {"t": "fox"}}], "slop": 0, "in_order": True}}}}},
    {"name": "span_containing", "q": {"span": {"containing": [{"near": [[{"t": "the"}, {"t": "dog"}], 5, True]}, {"t": "brown"}]}}, "es": {"span_containing": {"big": {"span_near": {"clauses": [{"span_term": {"t": "the"}}, {"span_term": {"t": "dog"}}], "slop": 5, "in_order": True}}, "little": {"span_term": {"t": "brown"}}}}},
    {"name": "span_within", "q": {"span": {"within": [{"near": [[{"t": "the"}, {"t": "dog"}], 5, True]}, {"t": "brown"}]}}, "es": {"span_within": {"big": {"span_near": {"clauses": [{"span_term": {"t": "the"}}, {"span_term": {"t": "dog"}}], "slop": 5, "in_order": True}}, "little": {"span_term": {"t": "brown"}}}}},
    {"name": "fs_weight", "q": {"fs": [{"term": ["t", "fox"]}, [{"weight": 2.0}], "multiply", "multiply"]}, "es": {"function_score": {"query": {"term": {"t": "fox"}}, "weight": 2.0}}},
    {"name": "fs_fvf", "q": {"fs": [{"term": ["t", "fox"]}, [{"fvf": ["n", 1.5, "log1p", None]}], "multiply", "sum"]}, "es": {"function_score": {"query": {"term": {"t": "fox"}}, "field_value_factor": {"field": "n", "factor": 1.5, "modifier": "log1p"}, "boost_mode": "sum"}}},
    {"name": "fs_filters", "q": {"fs": [{"all": True}, [{"weight": 3.0, "filter": {"term": ["k", "a"]}}, {"weight": 5.0, "filter": {"term": ["k", "b"]}}], "sum", "replace"]}, "es": {"function_score": {"query": {"match_all": {}}, "functions": [{"filter": {"term": {"k": "a"}}, "weight": 3}, {"filter": {"term": {"k": "b"}}, "weight": 5}], "score_mode": "sum", "boost_mode": "replace"}}},
    {"name": "fs_gauss", "q": {"fs": [{"term": ["t", "the"]}, [{"decay": ["gauss", "n", 10.0, 5.0, 0.5, 1.0]}], "multiply", "multiply"]}, "es": {"function_score": {"query": {"term": {"t": "the"}}, "gauss": {"n": {"origin": 10, "scale": 5, "decay": 0.5, "offset": 1}}}}},
    {"name": "fs_exp_lin", "q": {"fs": [{"all": True}, [{"decay": ["exp", "n", 0.0, 10.0, 0.5, 0.0]}, {"decay": ["linear", "n", 20.0, 15.0, 0.3, 0.0]}], "avg", "multiply"]}, "es": {"function_score": {"query": {"match_all": {}}, "functions": [{"exp": {"n": {"origin": 0, "scale": 10, "decay": 0.5}}}, {"linear": {"n": {"origin": 20, "scale": 15, "decay": 0.3}}}], "score_mode": "avg"}}},
    {"name": "fs_random", "q": {"fs": [{"all": True}, [{"random": [42, "n"]}], "multiply", "multiply"]}, "es": {"function_score": {"query": {"match_all": {}}, "random_score": {"seed": 42, "field": "n"}}}},
    {"name": "fs_minscore", "q": {"fs": [{"term": ["t", "fox"]}, [{"fvf": ["n", 1.0, "none", None]}], "multiply", "multiply", 20.0]}, "es": {"function_score": {"query": {"term": {"t": "fox"}}, "field_value_factor": {"field": "n"}, "min_score": 20}}},
]

def capture_search():
    out = []
    for c in CASES:
        r = req("POST", "/index-fx-main/_search", {"query": c["es"], "explain": True, "size": 20})
        if "error" in r:
            out.append({"name": c["name"], "q": c["q"], "error": r["error"]})
            continue
        hits = [{"id": h["_id"], "score": h["_score"], "explanation": h.get("_explanation")} for h in r["hits"]["hits"]]
        out.append({"name": c["name"], "q": c["q"], "total": r["hits"]["total"]["value"], "hits": hits})
    return out

HL_TEXTS = [
    ["The quick brown fox jumps over the lazy dog. A quick movement of the enemy will jeopardize six gunboats. All questions asked by five watched experts amaze the judge.", "Second value with quick fox"],
    ["alpha quick beta", "gamma quick delta", "no match here"],
    ["Brown <b>quick</b> & 'fox' / slash. Another sentence without it. Then quick again, with the fox!"],
    ["one two three four five six seven eight nine ten eleven twelve quick thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty quick fox twenty-one twenty-two twenty-three"],
]
HL_QUERIES = [
    ({"match": {"t": "quick fox"}}, {"terms": ["quick", "fox"]}),
    ({"match_phrase": {"t": "quick brown fox"}}, {"phrase": ["quick", "brown", "fox"]}),
    ({"prefix": {"t": "qu"}}, {"prefix": "qu"}),
    ({"match": {"t": "zzz"}}, {"terms": ["zzz"]}),
    ({"match_phrase": {"t": {"query": "quick fox", "slop": 2}}}, {"phrase": ["quick", "fox"], "slop": 2}),
    ({"fuzzy": {"t": {"value": "quack", "fuzziness": 1}}}, {"fuzzy": ["quack", 1]}),
    ({"wildcard": {"t": "f*x"}}, {"wildcard": "f*x"}),
    ({"bool": {"must": {"match": {"t": "brown"}}, "should": {"match": {"t": "dog"}}, "must_not": {"match": {"t": "zebra"}}}}, {"terms": ["brown", "dog"]}),
]
HL_OPTS = [
    {}, {"fragment_size": 20, "number_of_fragments": 2}, {"number_of_fragments": 0}, {"no_match_size": 8},
    {"order": "score", "number_of_fragments": 1}, {"boundary_scanner": "word"}, {"encoder": "html"},
    {"pre_tags": ["<b>"], "post_tags": ["</b>"]}, {"fragment_size": 30},
    {"type": "plain"}, {"type": "plain", "fragment_size": 20}, {"type": "plain", "number_of_fragments": 0},
    {"type": "plain", "fragmenter": "simple", "fragment_size": 25}, {"type": "plain", "no_match_size": 12},
    {"boundary_scanner": "word", "fragment_size": 15}, {"fragment_size": 10, "number_of_fragments": 3},
    {"type": "plain", "order": "score", "fragment_size": 15},
]

def capture_highlight():
    out = []
    req("DELETE", "/index-fx-hl")
    req("PUT", "/index-fx-hl", {"mappings": {"properties": {"t": {"type": "text"}}}})
    for i, vals in enumerate(HL_TEXTS):
        req("PUT", "/index-fx-hl/_doc/%d?refresh=true" % i, {"t": vals})
    for i, vals in enumerate(HL_TEXTS):
        for qes, qd in HL_QUERIES:
            for o in HL_OPTS:
                body = {"query": {"ids": {"values": [str(i)]}}, "highlight": {"highlight_query": qes, "fields": {"t": dict(o)}}}
                if "type" in o:
                    body["highlight"]["type"] = o["type"]
                    del body["highlight"]["fields"]["t"]["type"]
                r = req("POST", "/index-fx-hl/_search", body)
                hits = r.get("hits", {}).get("hits", [])
                frags = hits[0].get("highlight", {}).get("t", []) if hits else None
                out.append({"values": vals, "q": qd, "opts": o, "fragments": frags})
    req("DELETE", "/index-fx-hl")
    return out

# Phrases next to other clauses (bool should): which tokens a phrase highlights depends on
# whether it really matches (unified: Lucene's phrase matcher; plain: span near queries).
HLP_TEXTS = ["a c x b", "a b", "a a b", "b a x a", "x a y b z c", "c b a", "a b a b c", "a x x x b", "b b a a",
             "a b c a b c", "x b a c y"]
HLP_QUERIES = [(["a", "b", "c"], 2, "x"), (["a", "a"], 1, "b"), (["a", "b"], 1, "x"), (["a", "b"], 0, "x"),
               (["b", "a"], 2, None), (["a", "b", "c"], 0, None), (["a", "b"], 3, None), (["a", "x", "b"], 1, None),
               (["a", "a", "b"], 2, None), (["a", "b", "c"], 1, "y"), (["c", "a"], 1, None)]

def capture_hl_phrases():
    out = []
    req("DELETE", "/index-fx-hlp")
    req("PUT", "/index-fx-hlp", {"mappings": {"properties": {"t": {"type": "text"}}}})
    for i, v in enumerate(HLP_TEXTS):
        req("PUT", "/index-fx-hlp/_doc/%d?refresh=true" % i, {"t": v})
    for i, v in enumerate(HLP_TEXTS):
        for terms, slop, extra in HLP_QUERIES:
            should = [{"match_phrase": {"t": {"query": " ".join(terms), "slop": slop}}}]
            qd = [{"phrase": terms, "slop": slop}]
            if extra:
                should.append({"term": {"t": extra}})
                qd.append({"term": extra})
            for typ in ["unified", "plain"]:
                body = {"query": {"ids": {"values": [str(i)]}},
                        "highlight": {"type": typ, "highlight_query": {"bool": {"should": should}}, "fields": {"t": {}}}}
                r = req("POST", "/index-fx-hlp/_search", body)
                hits = r.get("hits", {}).get("hits", [])
                frags = hits[0].get("highlight", {}).get("t", []) if hits else None
                out.append({"values": [v], "q": {"should": qd}, "opts": {} if typ == "unified" else {"type": "plain"}, "fragments": frags})
    req("DELETE", "/index-fx-hlp")
    return out

# Rescore, collapse and terminate_after: `body` is sent as is; `q` / `rq` are rebuilt.
OR_QF = {"or": [{"term": ["t", "quick"]}, {"term": ["t", "fox"]}]}
OR_ES = {"match": {"t": "quick fox"}}
PH_Q = {"phrase": ["t", ["quick", "brown"], 0]}
PH_ES = {"match_phrase": {"t": "quick brown"}}
EXTRA = []
for mode in ["total", "multiply", "avg", "max", "min"]:
    EXTRA.append({"name": "rescore_" + mode, "kind": "rescore", "q": OR_QF, "rq": PH_Q, "window": 5, "qw": 0.7, "rqw": 1.2, "mode": mode,
                  "body": {"query": OR_ES, "rescore": {"window_size": 5, "query": {"rescore_query": PH_ES, "query_weight": 0.7, "rescore_query_weight": 1.2, "score_mode": mode}}}})
EXTRA.append({"name": "rescore_default", "kind": "rescore", "q": {"term": ["t", "the"]}, "rq": {"term": ["t", "dog"]}, "window": 10, "qw": 1.0, "rqw": 1.0, "mode": "total",
              "body": {"query": {"term": {"t": "the"}}, "rescore": {"query": {"rescore_query": {"term": {"t": "dog"}}}}}})
EXTRA.append({"name": "collapse_score", "kind": "collapse", "q": OR_QF, "sort": None, "body": {"query": OR_ES, "collapse": {"field": "g"}}})
EXTRA.append({"name": "collapse_all_n", "kind": "collapse", "q": {"all": True}, "sort": "n", "body": {"query": {"match_all": {}}, "collapse": {"field": "g"}, "sort": [{"n": "asc"}]}})
EXTRA.append({"name": "collapse_all_n_desc", "kind": "collapse", "q": {"all": True}, "sort": "-n", "size": 2, "body": {"query": {"match_all": {}}, "collapse": {"field": "g"}, "sort": [{"n": "desc"}], "size": 2}})
EXTRA.append({"name": "terminate_after", "kind": "terminate", "q": {"term": ["t", "the"]}, "max": 2, "body": {"query": {"term": {"t": "the"}}, "terminate_after": 2}})

# Intervals: `iv` is rebuilt with IntervalsSource factories (match rules are analyzed).
def m(q, max_gaps=-1, ordered=False, flt=None):
    es = {"match": {"query": q, "max_gaps": max_gaps, "ordered": ordered}}
    d = {"m": [q, max_gaps, ordered]}
    if flt:
        es["match"]["filter"] = {flt[0]: flt[1][0]}
        d = {"filter": [flt[0], d, flt[1][1]]}
    return es, d
def all_of(items, ordered=False, max_gaps=-1):
    return ({"all_of": {"intervals": [i[0] for i in items], "ordered": ordered, "max_gaps": max_gaps}},
            {"all_of": [[i[1] for i in items], ordered, max_gaps]})
def any_of(items):
    return ({"any_of": {"intervals": [i[0] for i in items]}}, {"any_of": [i[1] for i in items]})
IV = [
    ("iv_ordered", m("quick fox", ordered=True)),
    ("iv_unordered", m("fox quick")),
    ("iv_phrase", m("quick brown", 0, True)),
    ("iv_unordered_gaps", m("fox quick", 1)),
    ("iv_repeat", m("the the", ordered=True)),
    ("iv_repeat_unordered", m("the quick the")),
    ("iv_all_of", all_of([m("quick"), m("fox")], True, 2)),
    ("iv_any_of", any_of([m("lazy"), m("hound")])),
    ("iv_nested_or", all_of([any_of([m("quick"), m("brown")]), m("fox")], True)),
    ("iv_wide_or", all_of([any_of([m("quick brown", 0, True), m("lazy")]), m("dog")], True)),
    ("iv_prefix", ({"prefix": {"prefix": "qu"}}, {"prefix": "qu"})),
    ("iv_wildcard", ({"wildcard": {"pattern": "f*x"}}, {"wildcard": "f*x"})),
    ("iv_fuzzy", ({"fuzzy": {"term": "quack"}}, {"fuzzy": ["quack", 1]})),
    ("iv_not_containing", m("quick fox", ordered=True, flt=("not_containing", m("brown")))),
    ("iv_containing", all_of([m("the"), m("dog")], True, 6) if False else m("the dog", 6, True, ("containing", m("brown")))),
    ("iv_contained_by", m("brown", flt=("contained_by", m("quick dog", 6, True)))),
    ("iv_not_contained_by", m("brown", flt=("not_contained_by", m("quick dog", 6, True)))),
    ("iv_overlapping", m("quick fox", 3, True, ("overlapping", m("brown lazy", 8)))),
    ("iv_not_overlapping", m("quick fox", 3, True, ("not_overlapping", m("brown")))),
    ("iv_before", m("fox", flt=("before", m("lazy")))),
    ("iv_after", m("quick", flt=("after", m("the")))),
]
for name, (es, d) in IV:
    EXTRA.append({"name": name, "kind": "search", "q": {"iv": ["t", d]}, "body": {"query": {"intervals": {"t": es}}}})
# more_like_this: like texts / docs; `mlt` is rebuilt (like docs from the analyzed source).
MLT = [
    ("mlt_text", {"fields": ["t"], "like": "quick quick fox fox lazy lazy dog dog the the", "min_term_freq": 2, "min_doc_freq": 1}),
    ("mlt_text_boost", {"fields": ["t"], "like": "quick quick fox fox lazy lazy hound hound", "min_term_freq": 2, "min_doc_freq": 1, "boost_terms": 1.5, "max_query_terms": 3}),
    ("mlt_doc", {"fields": ["t"], "like": [{"_id": "0"}], "min_term_freq": 1, "min_doc_freq": 1}),
    ("mlt_docs_include", {"fields": ["t"], "like": [{"_id": "3"}, {"_id": "4"}], "min_term_freq": 1, "min_doc_freq": 2, "include": True, "minimum_should_match": "20%"}),
    ("mlt_unlike", {"fields": ["t"], "like": "quick fox lazy dog brown", "unlike": "fox", "min_term_freq": 1, "min_doc_freq": 1, "max_doc_freq": 6}),
]
EXTRA.append({"name": "terms_set_field", "kind": "search", "q": {"tset": ["k", ["a", "b", "c", "d"], "m"]},
              "body": {"query": {"terms_set": {"k": {"terms": ["a", "b", "c", "d"], "minimum_should_match_field": "m"}}}}})
for name, body in MLT:
    EXTRA.append({"name": name, "kind": "search", "q": {"mlt": body}, "body": {"query": {"more_like_this": body}}})

def capture_extra():
    out = []
    for c in EXTRA:
        body = dict(c["body"])
        body.setdefault("size", 20)
        body["explain"] = c["kind"] in ("rescore", "search")
        r = req("POST", "/index-fx-main/_search", body)
        if "error" in r:
            c2 = dict(c); c2["error"] = r["error"]; out.append(c2); continue
        hits = [{"id": h["_id"], "score": h["_score"], "sort": h.get("sort"), "fields": h.get("fields"), "explanation": h.get("_explanation")} for h in r["hits"]["hits"]]
        c2 = dict(c); del c2["body"]
        if c["kind"] == "search":
            v = req("POST", "/index-fx-main/_validate/query?rewrite=true", {"query": c["body"]["query"]})
            c2["rewritten"] = (v.get("explanations") or [{}])[0].get("explanation")
        c2.update({"total": r["hits"]["total"]["value"], "relation": r["hits"]["total"]["relation"], "terminated_early": r.get("terminated_early"), "hits": hits})
        out.append(c2)
    return out

# Sorting on the main index (k multi-valued keyword, n long, m long with gaps).
SORTS = [
    ("sort_n_asc", [{"n": "asc"}]),
    ("sort_n_desc", [{"n": "desc"}]),
    ("sort_k_min_asc", [{"k": {"order": "asc", "mode": "min"}}, {"n": "asc"}]),
    ("sort_k_max_desc_first", [{"k": {"order": "desc", "mode": "max", "missing": "_first"}}, {"n": "asc"}]),
    ("sort_m_missing_value", [{"m": {"order": "asc", "missing": 1}}, {"n": "desc"}]),
    ("sort_m_desc_last", [{"m": {"order": "desc"}}, {"n": "asc"}]),
    ("sort_score_n", [{"_score": "desc"}, {"n": "asc"}]),
]
def capture_sorts():
    out = []
    for name, sort in SORTS:
        q = {"match": {"t": "quick fox the"}} if "score" in name else {"match_all": {}}
        r = req("POST", "/index-fx-main/_search", {"query": q, "sort": sort, "size": 20, "track_scores": "score" in name})
        hits = [{"id": h["_id"], "sort": h["sort"], "score": h["_score"]} for h in r["hits"]["hits"]]
        after = hits[3]["sort"] if len(hits) > 4 else None
        r2 = req("POST", "/index-fx-main/_search", {"query": q, "sort": sort, "size": 20, "search_after": after}) if after else {"hits": {"hits": []}}
        out.append({"name": name, "sort": sort, "q": q, "hits": hits, "after": after, "after_hits": [h["_id"] for h in r2["hits"]["hits"]]})
    return out

# Nested: comments with text and stars.
NESTED_DOCS = [
    [("great quick product", 5), ("slow delivery", 2)],
    [("quick and nice", 4)],
    [],
    [("terrible", 1), ("quick quick fix", 3), ("nice nice", 5)],
    [("nice", 2), ("ok", 3)],
    [("quick", 1)],
]
def capture_nested():
    req("DELETE", "/index-fx-nested")
    r = req("PUT", "/index-fx-nested", {"mappings": {"properties": {"title": {"type": "keyword"}, "comments": {"type": "nested", "properties": {"text": {"type": "text"}, "stars": {"type": "long"}}}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, cs in enumerate(NESTED_DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"title": "doc%d" % i, "comments": [{"text": t, "stars": s} for t, s in cs]}))
    r = req("POST", "/index-fx-nested/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-nested/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-nested/_refresh")
    out = []
    for mode in ["avg", "max", "min", "sum", "none"]:
        body = {"query": {"nested": {"path": "comments", "query": {"match": {"comments.text": "quick nice"}}, "score_mode": mode}}, "explain": True}
        r = req("POST", "/index-fx-nested/_search", body)
        out.append({"name": "nested_" + mode, "mode": mode, "total": r["hits"]["total"]["value"],
                    "hits": [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in r["hits"]["hits"]]})
    for name, sort in [
        ("nsort_min", [{"comments.stars": {"order": "asc", "mode": "min", "nested": {"path": "comments"}}}]),
        ("nsort_max_desc", [{"comments.stars": {"order": "desc", "mode": "max", "nested": {"path": "comments"}}}]),
        ("nsort_avg_filter", [{"comments.stars": {"order": "asc", "mode": "avg", "nested": {"path": "comments", "filter": {"match": {"comments.text": "quick"}}}}}]),
        ("nsort_sum_missing", [{"comments.stars": {"order": "desc", "mode": "sum", "missing": 100, "nested": {"path": "comments"}}}]),
    ]:
        r = req("POST", "/index-fx-nested/_search", {"sort": sort, "size": 20})
        out.append({"name": name, "sort": sort, "hits": [{"id": h["_id"], "sort": h["sort"]} for h in r["hits"]["hits"]]})
    req("DELETE", "/index-fx-nested")
    return out

def capture_termvectors():
    out = []
    for i in ["0", "3", "6"]:
        r = req("GET", "/index-fx-main/_termvectors/" + i + "?fields=t&term_statistics=true&field_statistics=true&positions=true&offsets=true")
        out.append({"id": i, "tv": r["term_vectors"]["t"]})
    return out

FVH_TEXTS = HL_TEXTS + [
    ["the quick brown fox and the quick brown dog met a brown fox that was quick. quick brown foxes everywhere, brown fox quick fox."],
    ["", "quick fox at the start", "and quick again"],
    ["\u00dcn\u00efc\u00f6d\u00e9 quick \U0001F600 fox \u2014 tests \u2713 quick"],
]
FVH_QUERIES = HL_QUERIES + [
    ({"bool": {"should": [{"term": {"t": {"value": "quick", "boost": 1.0}}}, {"term": {"t": {"value": "fox", "boost": 3.0}}}]}}, {"boosted": [["quick", 1.0], ["fox", 3.0]]}),
    ({"bool": {"should": [{"match_phrase": {"t": "quick brown"}}, {"match_phrase": {"t": "brown fox"}}]}}, {"phrases": [["quick", "brown"], ["brown", "fox"]]}),
]
FVH_OPTS = [
    {}, {"fragment_size": 18}, {"fragment_size": 30, "number_of_fragments": 2}, {"number_of_fragments": 0},
    {"number_of_fragments": 1, "order": "score"}, {"no_match_size": 20}, {"no_match_size": 200},
    {"boundary_scanner": "word"}, {"boundary_scanner": "sentence"}, {"boundary_scanner": "sentence", "fragment_size": 40},
    {"boundary_chars": "o", "boundary_max_scan": 5}, {"fragment_offset": 2, "fragment_size": 20}, {"tags_schema": "styled"},
    {"pre_tags": ["<a>", "<b>"], "post_tags": ["</a>", "</b>"]}, {"encoder": "html"}, {"phrase_limit": 1},
    {"fragment_size": 17}, {"order": "score", "fragment_size": 25, "number_of_fragments": 3},
]

def capture_fvh():
    out = []
    req("DELETE", "/index-fx-fvh")
    req("PUT", "/index-fx-fvh", {"mappings": {"properties": {"t": {"type": "text", "term_vector": "with_positions_offsets"}}}})
    for i, vals in enumerate(FVH_TEXTS):
        req("PUT", "/index-fx-fvh/_doc/%d?refresh=true" % i, {"t": vals})
    for i, vals in enumerate(FVH_TEXTS):
        for qes, qd in FVH_QUERIES:
            for o in FVH_OPTS:
                f = dict(o)
                f["type"] = "fvh"
                body = {"query": {"ids": {"values": [str(i)]}}, "highlight": {"highlight_query": qes, "fields": {"t": f}}}
                r = req("POST", "/index-fx-fvh/_search", body)
                if "error" in r:
                    rc = r["error"]["root_cause"][0]
                    out.append({"values": vals, "q": qd, "opts": o, "error": [rc["type"], rc["reason"]]})
                    continue
                hits = r.get("hits", {}).get("hits", [])
                frags = hits[0].get("highlight", {}).get("t", []) if hits else None
                out.append({"values": vals, "q": qd, "opts": o, "fragments": frags})
    req("DELETE", "/index-fx-fvh")
    return out

IVG_ANALYSIS = {"filter": {"syn": {"type": "synonym_graph", "synonyms": [
    "ny, new york", "wifi, wi fi, wireless network", "nyc => new york city", "tv, television, telly set"]}},
    "analyzer": {"sg": {"tokenizer": "standard", "filter": ["lowercase", "syn"]}}}
IVG_DOCS = [
    "I love new york pizza", "ny pizza is great", "free wi fi in new york city", "wireless network pizza",
    "the wifi in ny is free", "new pizza in york", "a telly set and a tv in nyc", "television in new york city at night",
    "free pizza with wireless and network", "new york new york", "wi and fi and new and york",
]
IVG_QUERIES = [
    ("ny pizza", -1, False), ("ny pizza", 0, True), ("ny pizza", 2, True), ("free wifi in ny", 2, False),
    ("free wifi in ny", -1, True), ("nyc", -1, False), ("tv in nyc", 3, False), ("wifi", -1, False),
    ("new york", 0, True), ("telly set", -1, True), ("television ny", 4, True), ("pizza wifi", -1, False),
]

def capture_ivgraph():
    req("DELETE", "/index-fx-ivg")
    r = req("PUT", "/index-fx-ivg", {"settings": {"number_of_shards": 1, "analysis": IVG_ANALYSIS},
                                     "mappings": {"properties": {"t": {"type": "text"}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, t in enumerate(IVG_DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"t": t}))
    r = req("POST", "/index-fx-ivg/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-ivg/_forcemerge?max_num_segments=1")
    out = []
    for text, gaps, ordered in IVG_QUERIES:
        q = {"intervals": {"t": {"match": {"query": text, "analyzer": "sg", "max_gaps": gaps, "ordered": ordered}}}}
        v = req("POST", "/index-fx-ivg/_validate/query?explain=true", {"query": q})
        r = req("POST", "/index-fx-ivg/_search", {"query": q, "size": 20})
        out.append({"text": text, "max_gaps": gaps, "ordered": ordered, "lucene": v["explanations"][0]["explanation"],
                    "hits": [{"id": h["_id"], "score": h["_score"]} for h in r["hits"]["hits"]]})
    req("DELETE", "/index-fx-ivg")
    return {"analysis": IVG_ANALYSIS, "docs": IVG_DOCS, "cases": out}

# Completion suggester: three refreshes (three segments), weights, duplicates and a delete.
COMPLETION_BATCHES = [
    [("1", ["Nirvana", "Nevermind"], 34), ("2", ["Nirvana"], 10), ("3", ["Nine Inch Nails"], 20), ("4", ["The Smiths"], 15)],
    [("5", ["Nirvana"], 50), ("6", ["Nickelback"], 5), ("7", ["The Strokes", "Strokes"], 15), ("8", ["Nirvana"], 34)],
    [("9", ["Nirvana Unplugged"], 34), ("10", ["Nina Simone"], 3), ("11", ["The Stone Roses"], 15), ("12", ["Nevermind"], 99)],
]
COMPLETION_DELETE = ["12"]
COMPLETION_QUERIES = [
    ({"prefix": "n", "completion": {"field": "s", "size": 5}}, {"prefix": "n", "size": 5}),
    ({"prefix": "nir", "completion": {"field": "s", "size": 3}}, {"prefix": "nir", "size": 3}),
    ({"prefix": "nir", "completion": {"field": "s", "size": 5, "skip_duplicates": True}}, {"prefix": "nir", "size": 5, "skip_duplicates": True}),
    ({"prefix": "n", "completion": {"field": "s", "size": 4, "skip_duplicates": True}}, {"prefix": "n", "size": 4, "skip_duplicates": True}),
    ({"prefix": "nirv", "completion": {"field": "s", "size": 6, "fuzzy": {"fuzziness": 1}}}, {"prefix": "nirv", "size": 6, "fuzzy": 1}),
    ({"prefix": "nevr", "completion": {"field": "s", "size": 5, "fuzzy": {"fuzziness": 1}}}, {"prefix": "nevr", "size": 5, "fuzzy": 1}),
    ({"prefix": "the s", "completion": {"field": "s", "size": 5}}, {"prefix": "the s", "size": 5}),
    ({"prefix": "thes", "completion": {"field": "s", "size": 5}}, {"prefix": "thes", "size": 5}),
    ({"prefix": "zzz", "completion": {"field": "s", "size": 5}}, {"prefix": "zzz", "size": 5}),
]

def capture_completion():
    req("DELETE", "/index-fx-compl")
    r = req("PUT", "/index-fx-compl", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
                                       "mappings": {"properties": {"s": {"type": "completion"}}}})
    assert r.get("acknowledged"), r
    for batch in COMPLETION_BATCHES:
        lines = []
        for i, inputs, w in batch:
            lines.append(json.dumps({"index": {"_id": i}}))
            lines.append(json.dumps({"s": {"input": inputs, "weight": w}}))
        r = req("POST", "/index-fx-compl/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
        assert not r["errors"], r
    for i in COMPLETION_DELETE:
        req("DELETE", "/index-fx-compl/_doc/" + i + "?refresh=true")
    out = []
    for es, desc in COMPLETION_QUERIES:
        r = req("POST", "/index-fx-compl/_search", {"suggest": {"s": es}, "_source": False})
        opts = r["suggest"]["s"][0]["options"]
        out.append({"q": desc, "options": [{"text": o["text"], "id": o["_id"], "score": o["_score"]} for o in opts]})
    segs = req("GET", "/index-fx-compl/_segments")
    req("DELETE", "/index-fx-compl")
    nseg = len(segs["indices"]["index-fx-compl"]["shards"]["0"][0]["segments"])
    return {"batches": COMPLETION_BATCHES, "delete": COMPLETION_DELETE, "segments": nseg, "cases": out}

FEAT_DOCS = [
    {"t": "quick brown fox", "t2": "the fox", "pr": 10, "len": 3, "topics": {"pol": 20, "spo": 1.5}, "sv": {"a": 0.5, "b": 1.2}},
    {"t": "lazy dog", "t2": "quick quick quick dog and fox", "pr": 3.3, "len": 40, "topics": {"spo": 30}, "sv": {"a": 2.0}},
    {"t2": "fox", "pr": 0.001, "topics": {"pol": 0.7}, "sv": {"b": 0.1, "c": 3}},
    {"t": "fox fox the end of a long story about a fox", "len": 0.25, "sv": {"a": 7.5, "c": 0.01}},
    {"t": "the quick dog", "t2": "brown dog story", "pr": 120000, "topics": {"pol": 2, "eco": 9}},
    {"t": "a story", "pr": 1.0, "len": 1.0},
]

FEAT_QUERIES = [
    {"rank_feature": {"field": "pr"}},
    {"rank_feature": {"field": "pr", "saturation": {"pivot": 4}}},
    {"rank_feature": {"field": "pr", "saturation": {"pivot": 4}, "boost": 3}},
    {"rank_feature": {"field": "pr", "log": {"scaling_factor": 4}}},
    {"rank_feature": {"field": "pr", "sigmoid": {"pivot": 7, "exponent": 0.6}}},
    {"rank_feature": {"field": "pr", "linear": {}}},
    {"rank_feature": {"field": "len"}},
    {"rank_feature": {"field": "len", "sigmoid": {"pivot": 7, "exponent": 0.6}, "boost": 2}},
    {"rank_feature": {"field": "len", "linear": {}, "boost": 0.5}},
    {"rank_feature": {"field": "topics.pol"}},
    {"rank_feature": {"field": "topics.pol", "log": {"scaling_factor": 4}}},
    {"rank_feature": {"field": "topics.eco", "saturation": {"pivot": 9}}},
    {"rank_feature": {"field": "topics.none"}},
    {"sparse_vector": {"field": "sv", "query_vector": {"a": 1.5, "c": 0.2}}},
    {"sparse_vector": {"field": "sv", "query_vector": {"b": 3, "a": 0.25, "zz": 1}}},
    {"bool": {"must": [{"match": {"t": "fox"}}], "should": [{"rank_feature": {"field": "pr"}}]}},
    {"combined_fields": {"query": "quick fox", "fields": ["t", "t2^2"]}},
    {"combined_fields": {"query": "quick fox", "fields": ["t", "t2"]}},
    {"combined_fields": {"query": "dog story", "fields": ["t^1.5", "t2^3"], "operator": "and"}},
    {"combined_fields": {"query": "fox", "fields": ["t2", "t"]}},
    {"combined_fields": {"query": "brown", "fields": ["t^2.5"]}},
    {"combined_fields": {"query": "nothing", "fields": ["t", "t2"]}},
    {"combined_fields": {"query": "the dog", "fields": ["t", "t2"], "boost": 2}},
    {"match_phrase_prefix": {"t": "quick b"}},
    {"match_phrase_prefix": {"t2": "quick quick d"}},
    {"match_phrase_prefix": {"t": {"query": "fox the e", "slop": 1}}},
    {"match_phrase_prefix": {"t": {"query": "a s", "max_expansions": 1}}},
    {"match_phrase_prefix": {"t": "the zz"}},
    {"pinned": {"ids": ["3", "5"], "organic": {"match": {"t": "fox"}}}},
    {"pinned": {"ids": [], "organic": {"match": {"t": "quick"}}}},
    {"boosting": {"positive": {"match": {"t": "fox"}}, "negative": {"match": {"t2": "fox"}}, "negative_boost": 0.2}},
    {"boosting": {"positive": {"match": {"t": "fox"}}, "negative": {"match": {"t2": "fox"}}, "negative_boost": 0.5, "boost": 3}},
    {"boosting": {"positive": {"match": {"t": "the"}}, "negative": {"match": {"t": "zzz"}}, "negative_boost": 0.1}},
]

def capture_features():
    req("DELETE", "/index-fx-feat")
    r = req("PUT", "/index-fx-feat", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"t": {"type": "text"}, "t2": {"type": "text"}, "pr": {"type": "rank_feature"},
            "len": {"type": "rank_feature", "positive_score_impact": False}, "topics": {"type": "rank_features"},
            "sv": {"type": "sparse_vector"}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, d in enumerate(FEAT_DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps(d))
    r = req("POST", "/index-fx-feat/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-feat/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-feat/_refresh")
    out = []
    for q in FEAT_QUERIES:
        r = req("POST", "/index-fx-feat/_search", {"query": q, "explain": True, "size": 20})
        v = req("POST", "/index-fx-feat/_validate/query?explain=true", {"query": q})
        hits = [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in r["hits"]["hits"]]
        out.append({"es": q, "lucene": v["explanations"][0]["explanation"], "total": r["hits"]["total"]["value"], "hits": hits})
    req("DELETE", "/index-fx-feat")
    return {"docs": FEAT_DOCS, "cases": out, "distance": capture_distance(), "join": capture_join(), "geosort": capture_geosort(),
            "spell": capture_spell(), "phrase": capture_phrase(), "slices": capture_slices()}

def capture_slices():
    req("DELETE", "/index-fx-slice")
    r = req("PUT", "/index-fx-slice", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"n": {"type": "long"}}}})
    assert r.get("acknowledged"), r
    ids = [str(i) for i in range(40)] + ["abc", "x-y", "QmFzZTY0", "long id with spaces"]
    lines = []
    for k, i in enumerate(ids):
        lines.append(json.dumps({"index": {"_id": i}}))
        doc = {} if k % 7 == 3 else {"n": [k * 13 - 50, k] if k % 5 == 0 else k * 17 - 100}
        lines.append(json.dumps(doc))
    r = req("POST", "/index-fx-slice/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-slice/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-slice/_refresh")
    docs = []
    for k, i in enumerate(ids):
        docs.append({"id": i, "n": None if k % 7 == 3 else ([k * 13 - 50, k] if k % 5 == 0 else [k * 17 - 100])})
    out = []
    for kind in ["scroll", "field", "pit"]:
        for mx in [2, 3, 5]:
            for sid in range(mx):
                sl = {"id": sid, "max": mx}
                if kind == "field":
                    sl["field"] = "n"
                if kind == "pit":
                    pit = req("POST", "/index-fx-slice/_pit?keep_alive=1m")["id"]
                    r = req("POST", "/_search", {"pit": {"id": pit}, "slice": sl, "size": 100, "_source": False})
                    req("DELETE", "/_pit", {"id": pit})
                else:
                    r = req("POST", "/index-fx-slice/_search?scroll=1m", {"slice": sl, "size": 100, "_source": False})
                    if "_scroll_id" in r:
                        req("DELETE", "/_search/scroll", {"scroll_id": r["_scroll_id"]})
                out.append({"kind": kind, "id": sid, "max": mx, "ids": sorted(h["_id"] for h in r["hits"]["hits"])})
    req("DELETE", "/index-fx-slice")
    return {"docs": docs, "cases": out}

PHRASE_ANALYSIS = {
    "analyzer": {
        "body": {"tokenizer": "standard", "filter": ["lowercase"]},
        "bigram": {"tokenizer": "standard", "filter": ["lowercase", "bigram"]},
        "ngram": {"tokenizer": "standard", "filter": ["lowercase", "ngram"]},
        "trigram": {"tokenizer": "standard", "filter": ["lowercase", "trigram"]},
        "reverse": {"tokenizer": "standard", "filter": ["lowercase", "reverse"]},
    },
    "filter": {
        "bigram": {"type": "shingle", "output_unigrams": False, "min_shingle_size": 2, "max_shingle_size": 2},
        "ngram": {"type": "shingle", "output_unigrams": True, "min_shingle_size": 2, "max_shingle_size": 2},
        "trigram": {"type": "shingle", "output_unigrams": True, "min_shingle_size": 2, "max_shingle_size": 3},
    },
}

PHRASE_DOCS = ["Xorr the God-Jewel", "Xorn", "Arthur, King of the Britons", "Sir Lancelot the Brave", "Patsy, Arthur's Servant",
               "Sir Robin the Not-Quite-So-Brave-as-Sir-Lancelot", "Sir Bedevere the Wise", "Sir Galahad the Pure",
               "Miss Islington, the Witch", "Zoot", "Leader of Robin's Minstrels", "Old Crone", "Frank, the Historian",
               "Frank's Wife", "Dr. Piglet", "Dr. Winston", "Sir Robin (Stand-in)", "Knight Who Says Ni",
               "Police sergeant who stops the film", "the god jewel of king arthur", "king arthur and the knights",
               "sir lancelot and sir robin", "the brave sir robin ran away"]

def gen(field, **kw):
    g = {"field": field, "min_word_length": 1, "suggest_mode": "always"}
    g.update(kw)
    return g

PHRASE_CASES = [
    {"text": "xor the got-jewel", "phrase": {"field": "body.ngram", "force_unigrams": True, "max_errors": 0.5, "direct_generator": [gen("body.ngram")]}},
    {"text": "xor the got-jewel", "phrase": {"field": "body", "analyzer": "body", "force_unigrams": True, "max_errors": 0.5, "direct_generator": [gen("body")]}},
    {"text": "Artur, Ging of the Britons", "phrase": {"field": "body.ngram", "force_unigrams": True, "max_errors": 0.5,
        "direct_generator": [gen("body.reverse", pre_filter="reverse", post_filter="reverse")]}},
    {"text": "Artur, Ging of the Britons", "phrase": {"field": "body.ngram", "max_errors": 0.5, "confidence": 0,
        "direct_generator": [gen("body.ngram"), gen("body.reverse", pre_filter="reverse", post_filter="reverse")],
        "highlight": {"pre_tag": "<em>", "post_tag": "</em>"}}},
    {"text": "sir robbin the brav", "phrase": {"field": "body.ngram", "max_errors": 2, "size": 3, "direct_generator": [gen("body.ngram")]}},
    {"text": "sir robbin the brav", "phrase": {"field": "body.ngram", "max_errors": 2, "size": 3, "smoothing": {"laplace": {"alpha": 0.7}},
        "direct_generator": [gen("body.ngram")]}},
    {"text": "sir robbin the brav", "phrase": {"field": "body.ngram", "max_errors": 2, "size": 3,
        "smoothing": {"linear": {"trigram_lambda": 0.5, "bigram_lambda": 0.3, "unigram_lambda": 0.2}},
        "direct_generator": [gen("body.ngram")]}},
    {"text": "sir robbin the brav", "phrase": {"field": "body.ngram", "max_errors": 2, "size": 3, "smoothing": {"stupid_backoff": {"discount": 0.4}},
        "direct_generator": [gen("body.ngram")]}},
    {"text": "the brave sir robbin ran awy", "phrase": {"field": "body.trigram", "max_errors": 2, "size": 4, "real_word_error_likelihood": 0.8,
        "direct_generator": [gen("body.trigram", max_edits=1)]}},
    {"text": "the brave sir robbin ran awy", "phrase": {"field": "body.trigram", "max_errors": 2, "size": 4, "gram_size": 2,
        "smoothing": {"linear": {"trigram_lambda": 0.2, "bigram_lambda": 0.3, "unigram_lambda": 0.5}},
        "direct_generator": [gen("body.trigram")]}},
    {"text": "the brave sir robbin ran awy", "phrase": {"field": "body.trigram", "max_errors": 2, "size": 4,
        "smoothing": {"laplace": {"alpha": 0.2}}, "direct_generator": [gen("body.trigram")]}},
    {"text": "king artur", "phrase": {"field": "body.ngram", "direct_generator": [gen("body.ngram", suggest_mode="missing"), gen("body", suggest_mode="popular")]}},
    {"text": "king artur", "phrase": {"field": "body.ngram", "confidence": 2.0, "direct_generator": [gen("body.ngram")]}},
    {"text": "galahad the pur", "phrase": {"field": "body.ngram", "direct_generator": [gen("body.ngram", string_distance="jaro_winkler", size=2)]}},
]

def capture_phrase():
    req("DELETE", "/index-fx-phrase")
    r = req("PUT", "/index-fx-phrase", {"settings": {"number_of_shards": 1, "number_of_replicas": 0, "analysis": PHRASE_ANALYSIS},
        "mappings": {"properties": {"body": {"type": "text", "analyzer": "body", "fields": {
            "bigram": {"type": "text", "analyzer": "bigram"}, "ngram": {"type": "text", "analyzer": "ngram"},
            "trigram": {"type": "text", "analyzer": "trigram"}, "reverse": {"type": "text", "analyzer": "reverse"}}}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, d in enumerate(PHRASE_DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"body": d}))
    r = req("POST", "/index-fx-phrase/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-phrase/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-phrase/_refresh")
    out = []
    for c in PHRASE_CASES:
        r = req("POST", "/index-fx-phrase/_search", {"size": 0, "suggest": {"text": c["text"], "s": {"phrase": c["phrase"]}}})
        if "error" in r:
            out.append({"case": c, "error": r["error"]["root_cause"][0]["reason"]})
            continue
        opts = r["suggest"]["s"][0]["options"]
        out.append({"case": c, "options": [[o["text"], o.get("highlighted"), o["score"]] for o in opts]})
    req("DELETE", "/index-fx-phrase")
    return {"analysis": PHRASE_ANALYSIS, "docs": PHRASE_DOCS, "cases": out}

SPELL_WORDS = ["quick", "quack", "quirk", "quick", "brown", "browns", "crown", "frown", "grown", "brown",
               "dog", "dogs", "doge", "lazy", "laser", "hazy", "maze", "fox", "box", "foxes", "jumps", "bumps",
               "lumps", "elastic", "elastics", "plastic", "search", "searcher", "searches", "starch", "search",
               "elastic", "quick", "brown", "lazy", "lazily", "lazier", "lazies", "quickly", "quicker",
               "running", "runner", "running", "cunning", "stunning", "sunning", "searching", "sarching",
               "cat", "bat", "hat", "mat", "rat", "sat", "pat", "fat", "vat", "eat", "oat", "cot", "cut", "cab",
               "car", "can", "cap", "caw", "cast", "cart", "chat", "coat", "cats", "scat", "act", "cam", "tax",
               "wax", "max", "lax", "ax", "fax", "sax", "zax", "taxi", "cox", "crux", "calx", "carx"]

def spell_docs():
    import random
    rng = random.Random(42)
    docs = []
    for i in range(160):
        n = rng.randint(2, 6)
        docs.append(" ".join(rng.choice(SPELL_WORDS[: rng.randint(8, len(SPELL_WORDS))]) for _ in range(n)))
    return docs

SPELL_CASES = [
    {"text": "quikc", "term": {}},
    {"text": "brwn", "term": {}},
    {"text": "elastik serch", "term": {}},
    {"text": "lazzy", "term": {"suggest_mode": "always"}},
    {"text": "search", "term": {"suggest_mode": "popular"}},
    {"text": "lazy", "term": {"suggest_mode": "always", "sort": "frequency"}},
    {"text": "runing", "term": {"string_distance": "damerau_levenshtein"}},
    {"text": "runing", "term": {"string_distance": "levenshtein"}},
    {"text": "runing", "term": {"string_distance": "jaro_winkler"}},
    {"text": "runing", "term": {"string_distance": "ngram"}},
    {"text": "searchng", "term": {"max_edits": 1}},
    {"text": "qick", "term": {"prefix_length": 0}},
    {"text": "qick", "term": {"prefix_length": 2}},
    {"text": "fox", "term": {"min_word_length": 3, "suggest_mode": "always"}},
    {"text": "lumpz", "term": {"min_doc_freq": 3}},
    {"text": "lumpz", "term": {"min_doc_freq": 0.02}},
    {"text": "quick", "term": {"suggest_mode": "always", "max_term_freq": 0.5}},
    {"text": "quick", "term": {"suggest_mode": "always", "max_term_freq": 5}},
    {"text": "sunnning", "term": {"size": 2}},
    {"text": "starhc", "term": {"accuracy": 0.8}},
    {"text": "lazies", "term": {"suggest_mode": "always", "max_inspections": 1, "size": 3}},
    {"text": "cunnning", "term": {"size": 10}},
    {"text": "cax", "term": {"min_word_length": 3, "max_inspections": 1, "size": 3}},
    {"text": "cax", "term": {"min_word_length": 3, "max_inspections": 1, "size": 3, "max_term_freq": 0.5}},
    {"text": "caxt", "term": {"max_inspections": 1, "size": 2, "max_term_freq": 0.5}},
    {"text": "caxt", "term": {"max_inspections": 2, "size": 4, "max_term_freq": 0.5, "string_distance": "jaro_winkler"}},
    {"text": "xat", "term": {"min_word_length": 2, "prefix_length": 0, "max_inspections": 1, "size": 1}},
    {"text": "xat", "term": {"min_word_length": 2, "prefix_length": 0, "size": 20, "accuracy": 0.3}},
]

def capture_spell():
    req("DELETE", "/index-fx-spell")
    r = req("PUT", "/index-fx-spell", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"t": {"type": "text"}}}})
    assert r.get("acknowledged"), r
    docs = spell_docs()
    lines = []
    for i, d in enumerate(docs):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"t": d}))
    r = req("POST", "/index-fx-spell/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-spell/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-spell/_refresh")
    out = []
    for c in SPELL_CASES:
        term = dict(c["term"])
        term["field"] = "t"
        r = req("POST", "/index-fx-spell/_search", {"size": 0, "suggest": {"s": {"text": c["text"], "term": term}}})
        entries = [{"text": e["text"], "options": [[o["text"], o["score"], o["freq"]] for o in e["options"]]} for e in r["suggest"]["s"]]
        out.append({"case": c, "entries": entries})
    req("DELETE", "/index-fx-spell")
    return {"docs": docs, "cases": out}

GEOSORT_DOCS = [
    {"g": [1.0, 2.0], "n": 3},
    {"g": [[1.4, 2.6], [-3.0, -4.0]], "n": 10},
    {"n": 7},
    {"g": [179.9, 10.0], "n": -2},
    {"g": [[-179.95, 10.01], [100.0, -45.0], [0.0, 0.0]], "n": 5},
    {"g": [1.5, 2.5], "n": 1},
    {"g": [[50.0, 50.0], [-50.0, -50.0]]},
]

GEOSORT_CASES = [
    [{"_geo_distance": {"g": [1.5, 2.5]}}],
    [{"_geo_distance": {"g": [1.5, 2.5], "order": "desc"}}],
    [{"_geo_distance": {"g": [1.5, 2.5], "unit": "km", "distance_type": "plane"}}],
    [{"_geo_distance": {"g": [[1.5, 2.5], [-179.99, 10.0]], "mode": "max", "unit": "mi"}}],
    [{"_geo_distance": {"g": [0, 0], "mode": "avg"}}],
    [{"_geo_distance": {"g": [0, 0], "mode": "median", "order": "desc"}}],
    [{"_geo_distance": {"g": [0, 0], "mode": "min", "unit": "km", "distance_type": "plane"}}, {"n": "asc"}],
    [{"_script": {"type": "number", "script": {"source": "doc['n'].size() == 0 ? -1 : doc['n'].value * 2.5"}, "order": "desc"}}],
]

def capture_geosort():
    req("DELETE", "/index-fx-geosort")
    r = req("PUT", "/index-fx-geosort", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"g": {"type": "geo_point"}, "n": {"type": "long"}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, d in enumerate(GEOSORT_DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps(d))
    r = req("POST", "/index-fx-geosort/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-geosort/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-geosort/_refresh")
    out = []
    for sort in GEOSORT_CASES:
        r = req("POST", "/index-fx-geosort/_search", {"sort": sort, "size": 20})
        hits = [{"id": h["_id"], "sort": h["sort"]} for h in r["hits"]["hits"]]
        after = hits[2]["sort"] if len(hits) > 3 else None
        r2 = req("POST", "/index-fx-geosort/_search", {"sort": sort, "size": 20, "search_after": after}) if after else {"hits": {"hits": []}}
        out.append({"sort": sort, "hits": hits, "after": after, "after_hits": [h["_id"] for h in r2["hits"]["hits"]]})
    req("DELETE", "/index-fx-geosort")
    return {"docs": GEOSORT_DOCS, "cases": out}

JOIN_DOCS = [
    ("q1", None, {"t": "what fox", "join": "question"}),
    ("q2", None, {"t": "why dog", "join": "question"}),
    ("a1", "q1", {"t": "fox fox answer", "join": {"name": "answer", "parent": "q1"}}),
    ("a2", "q1", {"t": "a fox", "join": {"name": "answer", "parent": "q1"}}),
    ("a3", "q2", {"t": "fox dog", "join": {"name": "answer", "parent": "q2"}}),
    ("q3", None, {"t": "who fox fox", "join": "question"}),
    ("a4", "q3", {"t": "dog only", "join": {"name": "answer", "parent": "q3"}}),
    ("a5", "q2", {"t": "the quick fox jumps over a lazy fox", "join": {"name": "answer", "parent": "q2"}}),
    ("q4", None, {"t": "no answers fox", "join": "question"}),
]

JOIN_QUERIES = [
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}, "score_mode": "sum"}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}, "score_mode": "max"}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}, "score_mode": "min"}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}, "score_mode": "avg", "boost": 3}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}, "score_mode": "max", "min_children": 2}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "fox"}}, "max_children": 1}},
    {"has_child": {"type": "answer", "query": {"match": {"t": "zebra"}}, "score_mode": "max"}},
    {"has_parent": {"parent_type": "question", "query": {"match": {"t": "fox"}}}},
    {"has_parent": {"parent_type": "question", "query": {"match": {"t": "fox"}}, "score": True}},
    {"has_parent": {"parent_type": "question", "query": {"match": {"t": "fox"}}, "score": True, "boost": 2}},
]

def capture_join():
    req("DELETE", "/index-fx-join")
    r = req("PUT", "/index-fx-join", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"t": {"type": "text"}, "join": {"type": "join", "relations": {"question": "answer"}}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, routing, d in JOIN_DOCS:
        meta = {"_id": i}
        if routing:
            meta["routing"] = routing
        lines.append(json.dumps({"index": meta}))
        lines.append(json.dumps(d))
    r = req("POST", "/index-fx-join/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-join/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-join/_refresh")
    out = []
    for q in JOIN_QUERIES:
        r = req("POST", "/index-fx-join/_search", {"query": q, "explain": True, "size": 20})
        v = req("POST", "/index-fx-join/_validate/query?explain=true", {"query": q})
        w = req("POST", "/index-fx-join/_validate/query?rewrite=true", {"query": q})
        hits = [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in r["hits"]["hits"]]
        out.append({"es": q, "lucene": v["explanations"][0]["explanation"], "rewritten": w["explanations"][0]["explanation"],
                    "total": r["hits"]["total"]["value"], "hits": hits})
    req("DELETE", "/index-fx-join")
    return {"docs": JOIN_DOCS, "cases": out}

DIST_DOCS = [
    {"d": 1577836800000, "g": [1.0, 2.0]},
    {"d": [1577923200000, 1578009600000], "g": [[1.4, 2.6], [-3.0, -4.0]]},
    {"d": 1609459200000},
    {"g": [179.9, 10.0]},
    {"d": [1500000000000, 1650000000000, 1700000000000], "g": [[-179.95, 10.01], [100.0, -45.0]]},
    {"d": 1578268799999, "g": [1.5, 2.5]},
    {},
]

# (query, numeric origin/pivot for the Rust side)
DIST_QUERIES = [
    ({"distance_feature": {"field": "d", "origin": 1578268799999, "pivot": "2d"}}, {"long": [1578268799999, 172800000]}),
    ({"distance_feature": {"field": "d", "origin": 1600000000000, "pivot": "30d", "boost": 2.5}}, {"long": [1600000000000, 2592000000]}),
    ({"distance_feature": {"field": "g", "origin": [1.5, 2.5], "pivot": "10km"}}, {"geo": [2.5, 1.5, 10000.0]}),
    ({"distance_feature": {"field": "g", "origin": [-179.99, 10.0], "pivot": "1000km", "boost": 3}}, {"geo": [10.0, -179.99, 1000000.0]}),
]

def capture_distance():
    req("DELETE", "/index-fx-dist")
    r = req("PUT", "/index-fx-dist", {"settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"d": {"type": "date"}, "g": {"type": "geo_point"}}}})
    assert r.get("acknowledged"), r
    lines = []
    for i, d in enumerate(DIST_DOCS):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps(d))
    r = req("POST", "/index-fx-dist/_bulk?refresh=true", "\n".join(lines) + "\n", nd=True)
    assert not r["errors"], r
    req("POST", "/index-fx-dist/_forcemerge?max_num_segments=1")
    req("POST", "/index-fx-dist/_refresh")
    out = []
    for q, params in DIST_QUERIES:
        r = req("POST", "/index-fx-dist/_search", {"query": q, "explain": True, "size": 20})
        v = req("POST", "/index-fx-dist/_validate/query?explain=true", {"query": q})
        hits = [{"id": h["_id"], "score": h["_score"], "explanation": h["_explanation"]} for h in r["hits"]["hits"]]
        out.append({"es": q, "params": params, "lucene": v["explanations"][0]["explanation"], "total": r["hits"]["total"]["value"], "hits": hits})
    req("DELETE", "/index-fx-dist")
    return {"docs": DIST_DOCS, "cases": out}

ONLY = {"features": capture_features, "fvh": capture_fvh, "ivgraph": capture_ivgraph, "completion": capture_completion,
        "highlight_phrases": capture_hl_phrases}
if len(sys.argv) > 3 and sys.argv[3] in ONLY:
    json.dump(ONLY[sys.argv[3]](), open(OUT + "/" + sys.argv[3] + ".json", "w"), indent=0)
    print("ok")
    sys.exit(0)

setup()
json.dump(capture_fvh(), open(OUT + "/fvh.json", "w"), indent=0)
json.dump(capture_ivgraph(), open(OUT + "/ivgraph.json", "w"), indent=0)
json.dump(capture_completion(), open(OUT + "/completion.json", "w"), indent=0)
json.dump(capture_features(), open(OUT + "/features.json", "w"), indent=0)
json.dump(capture_sorts(), open(OUT + "/sorts.json", "w"), indent=0)
json.dump(capture_nested(), open(OUT + "/nested.json", "w"), indent=0)
json.dump(capture_termvectors(), open(OUT + "/termvectors.json", "w"), indent=0)
json.dump(capture_extra(), open(OUT + "/extra.json", "w"), indent=0)
json.dump(capture_search(), open(OUT + "/search.json", "w"), indent=0)
json.dump(capture_highlight(), open(OUT + "/highlight.json", "w"), indent=0)
json.dump(capture_hl_phrases(), open(OUT + "/highlight_phrases.json", "w"), indent=0)
req("DELETE", "/index-fx-main")
print("ok")
