"""opensearch-py against roll search's opensearch dialect: the shared client scenario through
the OpenSearch client and its helpers (bulk, scan, reindex).

Usage: python os_client.py   (OS_URL, default http://127.0.0.1:59201)
"""

import os
import sys
import traceback

from opensearchpy import NotFoundError, OpenSearch, RequestError, helpers

URL = os.environ.get("OS_URL", "http://127.0.0.1:59201")
IDX = "clients-ospy"
COLORS = ["red", "green", "blue"]
failures = []


def check(name):
    def wrap(fn):
        try:
            fn()
            print(f"ok   {name}", flush=True)
        except Exception as e:  # noqa: BLE001
            failures.append(name)
            print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
            traceback.print_exc()
        return fn

    return wrap


def eq(got, want, what=""):
    if got != want:
        raise AssertionError(f"{what}: expected {want!r}, got {got!r}")


client = OpenSearch(URL, timeout=60)


@check("info")
def _():
    info = client.info()
    eq(info["version"]["distribution"], "opensearch", "distribution")
    assert info["version"]["number"].startswith("3."), info


@check("create index")
def _():
    client.indices.delete(index=f"{IDX}*", ignore=[404])
    r = client.indices.create(
        index=IDX,
        body={
            "settings": {"number_of_shards": 2, "number_of_replicas": 0},
            "mappings": {"properties": {"title": {"type": "text"}, "tag": {"type": "keyword"}, "n": {"type": "integer"}}},
        },
    )
    eq(r["acknowledged"], True, "acknowledged")


@check("bulk helper")
def _():
    docs = ({"_index": IDX, "_id": str(i), "_source": {"title": f"doc {i} {'fox' if i % 2 else 'dog'}", "tag": COLORS[i % 3], "n": i}} for i in range(60))
    ok, errors = helpers.bulk(client, docs, refresh=True)
    eq((ok, errors), (60, []), "bulk")


@check("search with aggregations")
def _():
    r = client.search(index=IDX, body={"query": {"match": {"title": "fox"}}, "aggs": {"tags": {"terms": {"field": "tag"}}, "avg": {"avg": {"field": "n"}}}, "sort": [{"n": "desc"}], "size": 3})
    eq(r["hits"]["total"]["value"], 30, "total")
    eq([h["_id"] for h in r["hits"]["hits"]], ["59", "57", "55"], "ids")
    eq(r["aggregations"]["avg"]["value"], 30.0, "avg")


@check("scan helper")
def _():
    eq(sum(1 for _ in helpers.scan(client, index=IDX, size=9)), 60, "scanned")


@check("point in time")
def _():
    pit = client.create_pit(index=IDX, keep_alive="1m")["pit_id"]
    r = client.search(body={"pit": {"id": pit}, "size": 100, "sort": [{"n": "asc"}]})
    eq(len(r["hits"]["hits"]), 60, "hits")
    client.delete_pit(body={"pit_id": [pit]})


@check("update_by_query and delete_by_query")
def _():
    eq(client.update_by_query(index=IDX, refresh=True, body={"query": {"term": {"tag": "red"}}, "script": {"source": "ctx._source.n += 1000"}})["updated"], 20, "updated")
    eq(client.delete_by_query(index=IDX, refresh=True, body={"query": {"range": {"n": {"gte": 1000}}}})["deleted"], 20, "deleted")
    eq(client.count(index=IDX)["count"], 40, "count")


@check("errors")
def _():
    try:
        client.search(index="clients-ospy-missing")
        raise AssertionError("expected 404")
    except NotFoundError as e:
        eq(e.error, "index_not_found_exception", "type")
    try:
        client.search(index=IDX, body={"query": {"bogus": {}}})
        raise AssertionError("expected 400")
    except RequestError as e:
        eq(e.status_code, 400, "status")


@check("delete index")
def _():
    eq(client.indices.delete(index=f"{IDX}*")["acknowledged"], True, "deleted")


if failures:
    print(f"opensearch-py: {len(failures)} check(s) failed: {', '.join(failures)}")
    sys.exit(1)
print("opensearch-py: all checks passed")
