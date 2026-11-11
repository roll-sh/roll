"""Metabase against the server: its own database (Liquibase migrations at startup), then a
PostgreSQL database it syncs (tables, fields, types, keys, fingerprints, field values) and
queries (native SQL, MBQL with breakouts, filters and implicit joins, saved questions, an
X-ray). See metabase.sh."""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import apps  # noqa: E402
import psycopg  # noqa: E402

PORT = int(os.environ.get("APP_PORT", "52451"))
PGHOST, PGPORT = os.environ["PGHOST"], os.environ["PGPORT"]
m = apps.Http("http://127.0.0.1:%d" % PORT)
VOL = re.compile(r"^(id|.*_id|.*_at|entity_id|last_analyzed|running_time|average_execution_time|"
                 r"started_at|json_query|cache_.*|context|timezone|.*_timezone|position|creator|"
                 r"collection|dashboard_count|updated_at|created_at|last_used_at|view_count|"
                 r"moderation_reviews|can_.*|archived_directly|.*token|dataset_query|query_hash|"
                 # (Quartiles of a t-digest Metabase fills in its own order: they vary run to run.)
                 r"q1|q3)$")


def drop(k):
    return k in apps.VOLATILE or bool(VOL.match(k))


def step(name, method, path, body=None, keep=None):
    return m.step(name, method, path, body, drop=drop, keep=keep)


def rows_of(b):
    if not isinstance(b, dict):
        return b
    d = b.get("data") or {}
    return {"status": b.get("status"), "error": b.get("error"), "row_count": b.get("row_count"),
            "rows": d.get("rows"), "cols": [[c.get("name"), c.get("base_type"), c.get("database_type"),
                                             c.get("effective_type"), c.get("semantic_type")]
                                            for c in d.get("cols") or []],
            "native": (d.get("native_form") or {}).get("query")}


def main():
    t = time.time()
    if not apps.wait_http("http://127.0.0.1:%d/api/health" % PORT, 1800):
        apps.record("startup", "metabase never answered", ok=False)
        return
    apps.record("startup", "ok")
    print("metabase up after %.1fs" % (time.time() - t), flush=True)
    app = psycopg.connect(host=PGHOST, port=PGPORT, user="postgres", password="pw", dbname="metabase_app")
    app.autocommit = True
    apps.db_rows(app, "liquibase", "SELECT count(*), count(DISTINCT id), max(orderexecuted) FROM databasechangelog")
    apps.schema_digest(app)

    st, props = m.call("GET", "/api/session/properties")
    token = props.get("setup-token") if isinstance(props, dict) else None
    st, s = m.call("POST", "/api/setup", {
        "token": token, "prefs": {"site_name": "roll", "allow_tracking": False},
        "user": {"first_name": "Ann", "last_name": "Lee", "email": "ann@example.com",
                 "password": "Secret-pass-123", "site_name": "roll"}})
    apps.record("setup", {"status": st})
    sid = s.get("id") if isinstance(s, dict) else None
    m.headers["X-Metabase-Session"] = sid or ""
    st, db = step("db:add", "POST", "/api/database", {
        "engine": "postgres", "name": "shop", "is_full_sync": True, "is_on_demand": False,
        "details": {"host": PGHOST, "port": int(PGPORT), "dbname": "metabase_ds", "user": "postgres",
                    "password": "pw", "ssl": False, "schema-filters-type": "all"}},
        keep=lambda b: {k: b.get(k) for k in ("name", "engine")} if isinstance(b, dict) else b)
    dbid = db.get("id") if isinstance(db, dict) else 0
    end = time.time() + 900
    while time.time() < end:
        st, d = m.call("GET", "/api/database/%s" % dbid)
        if isinstance(d, dict) and d.get("initial_sync_status") in ("complete", "aborted"):
            break
        time.sleep(2)
    apps.record("db:sync", {"status": d.get("initial_sync_status") if isinstance(d, dict) else d,
                            "after": "%s" % (round(time.time() - end + 900) > 0)})
    print("sync done", flush=True)
    # Fingerprints and field values come from the analyze step after the sync: wait for them.
    end = time.time() + 600
    while time.time() < end:
        st, md = m.call("GET", "/api/database/%s/metadata?include_hidden=true" % dbid)
        fields = [f for t in (md.get("tables") or []) for f in t.get("fields") or []] if isinstance(md, dict) else []
        if fields and all(f.get("last_analyzed") for f in fields if f.get("visibility_type") != "sensitive"):
            break
        time.sleep(3)
    names = {}
    tables = {}
    for tb in sorted((md.get("tables") or []) if isinstance(md, dict) else [], key=lambda x: (x["schema"], x["name"])):
        tables[tb["name"]] = tb
        for f in tb.get("fields") or []:
            names[f["id"]] = "%s.%s" % (tb["name"], f["name"])
    for name, tb in sorted(tables.items()):
        fields = []
        for f in sorted(tb.get("fields") or [], key=lambda f: f.get("database_position", 0)):
            fields.append({k: f.get(k) for k in (
                "name", "display_name", "description", "database_type", "base_type", "effective_type",
                "semantic_type", "database_position", "database_required", "database_is_auto_increment",
                "has_field_values", "visibility_type", "nfc_path", "json_unfolding", "fingerprint")}
                | {"fk": names.get(f.get("fk_target_field_id"))})
        apps.record("table:" + name, apps.norm({k: tb.get(k) for k in (
            "schema", "display_name", "description", "entity_type", "visibility_type", "estimated_row_count")}
            | {"fields": fields}, drop))
    step("schemas", "GET", "/api/database/%s/schemas" % dbid)
    fid = {v: k for k, v in names.items()}
    tid = {n: t["id"] for n, t in tables.items()}
    for f in ("people.city", "people.tier", "products.category", "reviews.stars", "people.email"):
        if f in fid:
            step("values:" + f, "GET", "/api/field/%s/values" % fid[f], keep=lambda b: (b or {}).get("values"))
    nat = lambda name, sql: step("native:" + name, "POST", "/api/dataset",  # noqa: E731
                                 {"database": dbid, "type": "native", "native": {"query": sql}}, keep=rows_of)
    nat("count", "SELECT count(*) FROM orders")
    nat("join", "SELECT p.category, count(*), sum(o.total) FROM orders o JOIN products p ON p.id = o.product_id "
        "GROUP BY 1 ORDER BY 1")
    nat("types", "SELECT * FROM people ORDER BY id LIMIT 5")
    nat("json", "SELECT prefs->>'theme' AS theme, count(*) FROM people GROUP BY 1 ORDER BY 1")
    nat("error", "SELECT nope FROM orders")
    nat("view", "SELECT * FROM category_sales ORDER BY category")
    nat("schema", "SELECT * FROM analytics.daily ORDER BY day LIMIT 7")
    mb = lambda name, q: step("mbql:" + name, "POST", "/api/dataset",  # noqa: E731
                              {"database": dbid, "type": "query", "query": q}, keep=rows_of)
    F = lambda n, **o: ["field", fid.get(n, 0), o or None]  # noqa: E731
    mb("rows", {"source-table": tid.get("orders"), "limit": 10, "order-by": [["asc", F("orders.id")]]})
    mb("count_by_month", {"source-table": tid.get("orders"), "aggregation": [["count"]],
                          "breakout": [F("orders.placed", **{"temporal-unit": "month"})]})
    mb("sum_by_week", {"source-table": tid.get("orders"), "aggregation": [["sum", F("orders.total")]],
                       "breakout": [F("orders.placed", **{"temporal-unit": "week"})]})
    mb("avg_by_dow", {"source-table": tid.get("orders"), "aggregation": [["avg", F("orders.quantity")]],
                      "breakout": [F("orders.placed", **{"temporal-unit": "day-of-week"})]})
    mb("implicit_join", {"source-table": tid.get("orders"), "aggregation": [["count"], ["distinct", F("orders.person_id")]],
                         "breakout": [["field", fid.get("products.category", 0), {"source-field": fid.get("orders.product_id", 0)}]]})
    mb("filter", {"source-table": tid.get("people"), "aggregation": [["count"]],
                  "filter": ["and", ["=", F("people.city"), "Oslo", "Lima"], ["contains", F("people.email"), "1"],
                             ["between", F("people.born"), "1970-01-01", "2000-12-31"]],
                  "breakout": [F("people.tier")]})
    mb("binning", {"source-table": tid.get("products"), "aggregation": [["count"]],
                   "breakout": [["field", fid.get("products.price", 0), {"binning": {"strategy": "num-bins", "num-bins": 10}}]]})
    mb("expressions", {"source-table": tid.get("orders"), "expressions": {"unit": ["/", F("orders.total"), F("orders.quantity")]},
                       "aggregation": [["max", ["expression", "unit"]]], "breakout": [F("orders.quantity")]})
    mb("cumulative", {"source-table": tid.get("orders"), "aggregation": [["cum-count"]],
                      "breakout": [F("orders.placed", **{"temporal-unit": "quarter"})]})
    mb("percentile", {"source-table": tid.get("orders"), "aggregation": [["median", F("orders.total")],
                                                                          ["stddev", F("orders.total")]]})
    mb("relative_date", {"source-table": tid.get("reviews"), "aggregation": [["count"]],
                         "filter": ["time-interval", F("reviews.at"), -2000, "day"]})
    mb("nested", {"source-query": {"source-table": tid.get("orders"), "aggregation": [["count"]],
                                   "breakout": [F("orders.person_id")]},
                  "aggregation": [["avg", ["field", "count", {"base-type": "type/Integer"}]]]})
    # A saved question, run; search; an automatic dashboard of a table (many queries).
    st, card = m.call("POST", "/api/card", {"name": "by category", "display": "bar", "visualization_settings": {},
                                            "dataset_query": {"database": dbid, "type": "query", "query": {
                                                "source-table": tid.get("products"), "aggregation": [["count"]],
                                                "breakout": [F("products.category")]}}})
    cid = card.get("id") if isinstance(card, dict) else 0
    step("card:query", "POST", "/api/card/%s/query" % cid, keep=rows_of)
    step("search", "GET", "/api/search?q=categ", keep=lambda b: sorted(
        (x.get("model"), x.get("name")) for x in (b.get("data") or [])) if isinstance(b, dict) else b)
    step("xray", "GET", "/api/automagic-dashboards/table/%s" % tid.get("orders", 0),
         keep=lambda b: sorted(c.get("card", {}).get("name") or "" for c in b.get("dashcards") or [])
         if isinstance(b, dict) else b)
    step("sync_again", "POST", "/api/database/%s/sync_schema" % dbid)
    apps.db_rows(app, "rows:fields", "SELECT t.name, f.name, f.base_type, f.semantic_type, f.database_type "
                 "FROM metabase_field f JOIN metabase_table t ON t.id = f.table_id WHERE f.active "
                 "ORDER BY 1, 2")
    apps.db_rows(app, "rows:tables", "SELECT schema, name, entity_type, active FROM metabase_table ORDER BY 1, 2")


if __name__ == "__main__":
    main()
