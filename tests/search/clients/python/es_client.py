"""elasticsearch-py 9.x against roll search: the representative scenario every client in
tests/search/clients runs (index with mappings, bulk helpers, search with aggregations, scroll
and point in time, by-query APIs, errors), plus the client's product check, which runs on the
first request and refuses servers that don't identify as Elasticsearch.

Usage: python es_client.py   (ES_URL, default http://127.0.0.1:59200)
"""

import os
import sys
import traceback

from elasticsearch import BadRequestError, ConflictError, Elasticsearch, NotFoundError, helpers

URL = os.environ.get("ES_URL", "http://127.0.0.1:59200")
IDX = "clients-py"
COLORS = ["red", "green", "blue"]
failures = []


def check(name):
    def wrap(fn):
        try:
            fn()
            print(f"ok   {name}", flush=True)
        except Exception as e:  # noqa: BLE001 - every failure is reported, then the run fails
            failures.append(name)
            print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
            traceback.print_exc()
        return fn

    return wrap


def eq(got, want, what=""):
    if got != want:
        raise AssertionError(f"{what}: expected {want!r}, got {got!r}")


es = Elasticsearch(URL, request_timeout=60)


@check("info and product check")
def _():
    info = es.info()
    eq(info["version"]["number"], "9.5.5", "version")
    eq(info["tagline"], "You Know, for Search", "tagline")
    assert es.ping()


@check("create index with mappings")
def _():
    es.options(ignore_status=404).indices.delete(index=f"{IDX}*")
    r = es.indices.create(
        index=IDX,
        settings={"number_of_shards": 2, "number_of_replicas": 0},
        mappings={
            "properties": {
                "title": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
                "tag": {"type": "keyword"},
                "n": {"type": "integer"},
                "price": {"type": "double"},
                "d": {"type": "date"},
                "comments": {"type": "nested", "properties": {"author": {"type": "keyword"}, "stars": {"type": "integer"}}},
            }
        },
    )
    eq(r["acknowledged"], True, "acknowledged")
    eq(r["index"], IDX, "index")
    assert es.indices.exists(index=IDX)
    m = es.indices.get_mapping(index=IDX)
    eq(m[IDX]["mappings"]["properties"]["tag"], {"type": "keyword"}, "mapping")


def doc(i):
    return {
        "title": f"document {i} {'quick brown fox' if i % 2 else 'lazy dog'}",
        "tag": COLORS[i % 3],
        "n": i,
        "price": i * 1.5,
        "d": f"2024-{i % 12 + 1:02d}-{i % 28 + 1:02d}",
        "comments": [{"author": f"a{i % 5}", "stars": i % 5}],
    }


@check("bulk helper")
def _():
    ok, errors = helpers.bulk(es, ({"_index": IDX, "_id": str(i), "_source": doc(i)} for i in range(100)), refresh=True)
    eq(ok, 100, "indexed")
    eq(errors, [], "errors")
    eq(es.count(index=IDX)["count"], 100, "count")


@check("streaming_bulk and parallel_bulk")
def _():
    n = sum(1 for ok, _ in helpers.streaming_bulk(es, ({"_index": f"{IDX}-extra", "_id": i, "x": i} for i in range(30)), chunk_size=7) if ok)
    eq(n, 30, "streaming_bulk")
    n = sum(1 for ok, _ in helpers.parallel_bulk(es, ({"_op_type": "delete", "_index": f"{IDX}-extra", "_id": i} for i in range(10)), chunk_size=4) if ok)
    eq(n, 10, "parallel_bulk deletes")
    es.indices.refresh(index=f"{IDX}-extra")
    eq(es.count(index=f"{IDX}-extra")["count"], 20, "count after deletes")


@check("search with query and aggregations")
def _():
    r = es.search(
        index=IDX,
        query={"bool": {"must": [{"match": {"title": "fox"}}], "filter": [{"range": {"n": {"gte": 10, "lt": 60}}}]}},
        aggs={
            "tags": {"terms": {"field": "tag"}, "aggs": {"avg_n": {"avg": {"field": "n"}}}},
            "months": {"date_histogram": {"field": "d", "calendar_interval": "month"}},
            "price": {"stats": {"field": "price"}},
            "stars": {"nested": {"path": "comments"}, "aggs": {"max": {"max": {"field": "comments.stars"}}}},
        },
        size=5,
        sort=[{"n": "asc"}],
        highlight={"fields": {"title": {}}},
    )
    eq(r["hits"]["total"], {"value": 25, "relation": "eq"}, "total")
    eq([h["_id"] for h in r["hits"]["hits"]], ["11", "13", "15", "17", "19"], "ids")
    eq(r["hits"]["hits"][0]["highlight"]["title"], ["document 11 quick brown <em>fox</em>"], "highlight")
    buckets = {b["key"]: (b["doc_count"], b["avg_n"]["value"]) for b in r["aggregations"]["tags"]["buckets"]}
    eq(buckets, {"red": (8, 36.0), "green": (8, 34.0), "blue": (9, 35.0)}, "terms")
    eq(r["aggregations"]["price"]["count"], 25, "stats.count")
    eq(r["aggregations"]["price"]["sum"], sum(i * 1.5 for i in range(11, 60, 2)), "stats.sum")
    eq(r["aggregations"]["stars"]["max"]["value"], 4.0, "nested max")
    eq(sum(b["doc_count"] for b in r["aggregations"]["months"]["buckets"]), 25, "date_histogram")


@check("documents: get, exists, mget, update, delete, conflicts")
def _():
    g = es.get(index=IDX, id="5")
    eq(g["_source"]["n"], 5, "get")
    eq(g["_version"], 1, "version")
    assert es.exists(index=IDX, id="5")
    assert not es.exists(index=IDX, id="nope")
    m = es.mget(index=IDX, ids=["1", "2", "nope"])
    eq([d["found"] for d in m["docs"]], [True, True, False], "mget")
    u = es.update(index=IDX, id="5", doc={"n": 500}, refresh=True)
    eq(u["result"], "updated", "update")
    eq(es.get(index=IDX, id="5")["_source"]["n"], 500, "updated value")
    eq(es.get_source(index=IDX, id="5")["n"], 500, "get_source")
    try:
        es.index(index=IDX, id="5", document={"n": 1}, if_seq_no=0, if_primary_term=1)
        raise AssertionError("expected a version conflict")
    except ConflictError as e:
        eq(e.meta.status, 409, "status")
        eq(e.body["error"]["type"], "version_conflict_engine_exception", "type")
    try:
        es.get(index=IDX, id="nope")
        raise AssertionError("expected 404")
    except NotFoundError as e:
        eq(e.body["found"], False, "found")
    es.index(index=IDX, id="tmp", document={"n": -1}, refresh="wait_for")
    eq(es.delete(index=IDX, id="tmp", refresh=True)["result"], "deleted", "delete")


@check("scan helper (scroll)")
def _():
    ids = {h["_id"] for h in helpers.scan(es, index=IDX, query={"query": {"match_all": {}}}, size=7, scroll="1m")}
    eq(len(ids), 100, "scanned")
    # Raw scroll API, cleared at the end.
    r = es.search(index=IDX, scroll="1m", size=40, sort=["_doc"])
    seen = len(r["hits"]["hits"])
    sid = r["_scroll_id"]
    while True:
        r = es.scroll(scroll_id=sid, scroll="1m")
        if not r["hits"]["hits"]:
            break
        seen += len(r["hits"]["hits"])
        sid = r["_scroll_id"]
    eq(seen, 100, "scrolled")
    eq(es.clear_scroll(scroll_id=sid)["succeeded"], True, "clear_scroll")


@check("point in time with search_after")
def _():
    pit = es.open_point_in_time(index=IDX, keep_alive="1m")["id"]
    seen, after = [], None
    while True:
        kw = {"search_after": after} if after else {}
        r = es.search(pit={"id": pit, "keep_alive": "1m"}, size=30, sort=[{"n": "asc"}, {"_shard_doc": "asc"}], **kw)
        hits = r["hits"]["hits"]
        if not hits:
            break
        seen += [h["_id"] for h in hits]
        after = hits[-1]["sort"]
        pit = r["pit_id"]
    eq(len(seen), 100, "paged")
    eq(len(set(seen)), 100, "distinct")
    eq(seen[:3], ["0", "1", "2"], "order")
    eq(es.close_point_in_time(id=pit)["succeeded"], True, "close")


@check("msearch and count")
def _():
    r = es.msearch(searches=[{"index": IDX}, {"query": {"term": {"tag": "red"}}, "size": 0}, {"index": IDX}, {"query": {"term": {"tag": "nope"}}}])
    eq([x["hits"]["total"]["value"] for x in r["responses"]], [34, 0], "msearch totals")
    eq(es.count(index=IDX, query={"term": {"tag": "blue"}})["count"], 33, "count")


@check("update_by_query and delete_by_query")
def _():
    r = es.update_by_query(index=IDX, query={"term": {"tag": "blue"}}, script={"source": "ctx._source.n += params.k", "params": {"k": 1000}}, refresh=True)
    eq(r["updated"], 33, "updated")
    eq(es.count(index=IDX, query={"range": {"n": {"gte": 1000}}})["count"], 33, "after update_by_query")
    r = es.delete_by_query(index=IDX, query={"term": {"tag": "green"}}, refresh=True)
    eq(r["deleted"], 33, "deleted")
    eq(es.count(index=IDX)["count"], 67, "after delete_by_query")


@check("reindex helper and aliases")
def _():
    helpers.reindex(es, IDX, f"{IDX}-copy")
    es.indices.refresh(index=f"{IDX}-copy")
    eq(es.count(index=f"{IDX}-copy")["count"], 67, "reindexed")
    es.indices.put_alias(index=f"{IDX}-copy", name=f"{IDX}-alias")
    eq(es.count(index=f"{IDX}-alias")["count"], 67, "alias")
    eq(list(es.indices.get_alias(name=f"{IDX}-alias").keys()), [f"{IDX}-copy"], "get_alias")


@check("errors map to exceptions")
def _():
    try:
        es.search(index="clients-py-missing")
        raise AssertionError("expected 404")
    except NotFoundError as e:
        eq(e.body["error"]["type"], "index_not_found_exception", "type")
        eq(e.body["error"]["reason"], "no such index [clients-py-missing]", "reason")
    try:
        es.search(index=IDX, query={"bogus": {}})
        raise AssertionError("expected 400")
    except BadRequestError as e:
        eq(e.body["error"]["type"], "parsing_exception", "type")
        eq(e.body["error"]["reason"], "unknown query [bogus]", "reason")


@check("cat and cluster APIs")
def _():
    eq(es.cluster.health(index=IDX)["status"], "green", "health")
    rows = es.cat.indices(index=f"{IDX}*", format="json", h="index,docs.count", s="index")
    eq([r["index"] for r in rows], [IDX, f"{IDX}-copy", f"{IDX}-extra"], "cat indices")


@check("delete indices")
def _():
    eq(es.indices.delete(index=f"{IDX}*")["acknowledged"], True, "deleted")
    assert not es.indices.exists(index=IDX)


es.close()
if failures:
    print(f"elasticsearch-py: {len(failures)} check(s) failed: {', '.join(failures)}")
    sys.exit(1)
print("elasticsearch-py: all checks passed")
