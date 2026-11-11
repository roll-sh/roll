"""Hasura GraphQL Engine against the server: its metadata database (hdb_catalog) and a source
(hasura_ds, hasura.setup.sql) whose tables, relationships, functions, computed fields,
permissions and event triggers it tracks; then the GraphQL schema's introspection, queries
(filters, ordering, paging, nested relationships, aggregates, distinct_on, functions,
by-primary-key, role permissions) and mutations (inserts with nested objects, upserts,
updates with _inc / _append, deletes, the constraint errors they hit). See hasura.sh."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import apps  # noqa: E402

PORT = int(os.environ.get("APP_PORT", "52452"))
h = apps.Http("http://127.0.0.1:%d" % PORT, headers={"X-Hasura-Admin-Secret": "secret"})
DROP = apps.VOLATILE - {"id", "key"}  # (ids here are the fixture's serials: deterministic)


def meta(name, body, keep=None):
    return h.step("meta:" + name, "POST", "/v1/metadata", body, drop=DROP, keep=keep)


def sql(name, text):
    return h.step("sql:" + name, "POST", "/v2/query",
                  {"type": "run_sql", "args": {"source": "default", "sql": text, "read_only": True}}, drop=DROP)


def gql(name, query, variables=None, role=None, keep=None):
    hd = {"X-Hasura-Role": role, "X-Hasura-User-Id": "1"} if role else None
    return h.step("gql:" + name, "POST", "/v1/graphql", {"query": query, "variables": variables or {}},
                  drop=DROP, headers=hd, keep=keep)


def main():
    t = time.time()
    if not apps.wait_http("http://127.0.0.1:%d/healthz" % PORT, 600):
        apps.record("startup", "hasura never answered", ok=False)
        return
    apps.record("startup", "ok")
    print("hasura up after %.1fs" % (time.time() - t), flush=True)
    tables = ["article_status", "authors", "articles", "tags", "article_tags", "comments", "author_stats"]
    meta("track", {"type": "bulk", "args": [
        {"type": "pg_track_table", "args": {"source": "default", "table": {"schema": "public", "name": n},
                                            **({"is_enum": True} if n == "article_status" else {})}}
        for n in tables]})
    meta("suggest", {"type": "pg_suggest_relationships", "version": 1,
                     "args": {"source": "default", "omit_tracked": False}},
         keep=lambda b: sorted(str(r.get("from")) + str(r.get("to")) for r in (b or {}).get("relationships", []))
         if isinstance(b, dict) else b)
    rels = [
        ("pg_create_object_relationship", "articles", "author", {"foreign_key_constraint_on": "author_id"}),
        ("pg_create_array_relationship", "authors", "articles",
         {"foreign_key_constraint_on": {"table": "articles", "column": "author_id"}}),
        ("pg_create_array_relationship", "articles", "comments",
         {"foreign_key_constraint_on": {"table": "comments", "column": "article_id"}}),
        ("pg_create_array_relationship", "articles", "tags",
         {"foreign_key_constraint_on": {"table": "article_tags", "column": "article_id"}}),
        ("pg_create_object_relationship", "article_tags", "tag", {"foreign_key_constraint_on": "tag_id"}),
        ("pg_create_object_relationship", "comments", "article", {"foreign_key_constraint_on": "article_id"}),
        ("pg_create_object_relationship", "author_stats", "author",
         {"manual_configuration": {"remote_table": "authors", "column_mapping": {"id": "id"}}}),
    ]
    meta("relationships", {"type": "bulk", "args": [
        {"type": t, "args": {"source": "default", "table": tb, "name": n, "using": u}} for t, tb, n, u in rels]})
    meta("functions", {"type": "bulk", "args": [
        {"type": "pg_track_function", "args": {"source": "default", "function": "search_articles"}},
        {"type": "pg_add_computed_field", "args": {"source": "default", "table": "articles", "name": "words",
                                                   "definition": {"function": "article_words"}}}]})
    meta("permissions", {"type": "bulk", "args": [
        {"type": "pg_create_select_permission", "args": {
            "source": "default", "table": "articles", "role": "user",
            "permission": {"columns": ["id", "title", "status", "views"], "filter": {"author_id": {"_eq": "X-Hasura-User-Id"}},
                           "allow_aggregations": True, "limit": 5}}},
        {"type": "pg_create_insert_permission", "args": {
            "source": "default", "table": "comments", "role": "user",
            "permission": {"check": {"score": {"_gte": 0}}, "columns": ["article_id", "text", "score"],
                           "set": {"who": "X-Hasura-User-Id"}}}}]})
    meta("event_trigger", {"type": "pg_create_event_trigger", "args": {
        "source": "default", "name": "article_changed", "table": {"schema": "public", "name": "articles"},
        "webhook": "http://127.0.0.1:9/never", "insert": {"columns": "*"}, "update": {"columns": ["status"]},
        "retry_conf": {"num_retries": 0, "interval_sec": 10, "timeout_sec": 1}}})
    meta("inconsistent", {"type": "get_inconsistent_metadata", "args": {}})
    meta("export", {"type": "export_metadata", "version": 2, "args": {}},
         keep=lambda b: (b or {}).get("metadata", {}).get("sources") if isinstance(b, dict) else b)
    meta("reload", {"type": "reload_metadata", "args": {"reload_sources": True}})
    gql("introspection", INTROSPECTION, keep=lambda b: b)
    gql("by_pk", "{ authors_by_pk(id: 1) { name email bio rating } }")
    gql("where", "{ articles(where: {_and: [{views: {_gt: 200}}, {status: {_neq: published}}]}, order_by: {views: desc}) "
        "{ id title views status } }")
    gql("paging", "{ articles(order_by: [{author_id: asc}, {id: desc}], limit: 5, offset: 3) { id author_id } }")
    gql("nested", "{ authors(order_by: {name: asc}) { name articles(order_by: {id: asc}, limit: 2) "
        "{ title tags(order_by: {tag_id: asc}) { tag { name } } comments_aggregate { aggregate { count avg { score } } } } } }")
    gql("aggregate", "{ articles_aggregate(where: {status: {_eq: published}}) { aggregate { count sum { views } "
        "avg { views } max { views title } min { published_on } stddev { views } variance { views } } } }")
    gql("group_like", "{ authors { name articles_aggregate(distinct_on: status) { aggregate { count(columns: status, distinct: true) } } } }")
    gql("distinct_on", "{ comments(distinct_on: who, order_by: [{who: asc}, {score: desc}]) { who score } }")
    gql("json", "{ authors(where: {bio: {_has_key: \"awards\"}}, order_by: {id: asc}) { name bio(path: \"$.awards\") } }")
    gql("json_contains", "{ authors(where: {bio: {_contains: {country: \"IT\"}}}) { name } }")
    gql("text_ops", "{ a: articles(where: {title: {_ilike: \"%1_\"}}, order_by: {id: asc}) { title } "
        "b: articles(where: {title: {_regex: \"^article [12]$\"}}, order_by: {id: asc}) { title } "
        "c: articles(where: {title: {_similar: \"article (5|6)\"}}) { title } }")
    gql("array_ops", "{ articles(where: {labels: {_contains: [\"l1\"]}}, order_by: {id: asc}) { id labels } }")
    gql("in_null", "{ authors(where: {_or: [{email: {_is_null: true}}, {id: {_in: [2, 3]}}]}, order_by: {id: asc}) { id name } }")
    gql("exists", "{ authors(where: {articles: {comments: {score: {_gte: 5}}}}, order_by: {id: asc}) { name } }")
    gql("function", "{ search_articles(args: {q: \"article 2\"}, order_by: {id: asc}) { id title words } }")
    gql("computed", "{ articles(where: {id: {_lte: 3}}, order_by: {id: asc}) { id words } }")
    gql("enum", "{ articles(where: {status: {_eq: archived}}, limit: 3, order_by: {id: asc}) { id status } }")
    gql("view", "{ author_stats(order_by: {views: desc}) { name articles views author { email } } }")
    gql("variables", "query Q($min: bigint!, $n: Int!) { articles(where: {views: {_gte: $min}}, limit: $n, order_by: {id: asc}) "
        "{ id } }", {"min": 500, "n": 4})
    gql("role_select", "{ articles(order_by: {id: asc}) { id title status } articles_aggregate { aggregate { count } } }",
        role="user")
    gql("role_denied", "{ articles { body } }", role="user")
    gql("insert_nested", "mutation { insert_authors_one(object: {name: \"Octavia\", email: \"ob@example.com\", "
        "bio: {country: \"US\"}, articles: {data: [{title: \"Kindred\", status: published, labels: [\"sf\"]}, "
        "{title: \"Dawn\"}]}}) { name articles(order_by: {title: asc}) { title status labels } } }")
    gql("upsert", "mutation { insert_tags(objects: [{name: \"sf\"}, {name: \"fantasy\"}], on_conflict: "
        "{constraint: tags_name_key, update_columns: [name]}) { affected_rows returning { name } } }")
    gql("update_inc", "mutation { update_articles(where: {id: {_lte: 4}}, _inc: {views: 10}, _set: {status: published}) "
        "{ affected_rows returning { id views status } } }")
    gql("update_append", "mutation { update_authors_by_pk(pk_columns: {id: 2}, _append: {bio: {awards: [\"Viareggio\"]}}) "
        "{ bio } }")
    gql("update_many", "mutation { update_articles_many(updates: [{where: {id: {_eq: 5}}, _set: {title: \"five\"}}, "
        "{where: {id: {_eq: 6}}, _inc: {views: -1}}]) { affected_rows } }")
    gql("delete", "mutation { delete_comments(where: {score: {_lt: -3}}) { affected_rows } }")
    gql("err_unique", "mutation { insert_authors_one(object: {name: \"Italo\"}) { id } }")
    gql("err_check", "mutation { insert_authors_one(object: {name: \"Bad\", email: \"nope\"}) { id } }")
    gql("err_fk", "mutation { insert_articles_one(object: {author_id: 999, title: \"x\"}) { id } }")
    gql("err_enum_fk", "mutation { insert_comments_one(object: {article_id: 1, who: \"w\", text: \"t\", score: 9}) { id } }")
    gql("role_insert", "mutation { insert_comments_one(object: {article_id: 2, text: \"mine\", score: 1}) { id } }", role="user")
    gql("role_insert_check", "mutation { insert_comments_one(object: {article_id: 2, text: \"neg\", score: -1}) { id } }",
        role="user")
    gql("after", "{ comments_aggregate { aggregate { count sum { score } } } authors_aggregate { aggregate { count } } "
        "tags(order_by: {name: asc}) { name } }")
    sql("events", "SELECT trigger_name, count(*) FROM hdb_catalog.event_log GROUP BY 1")
    sql("triggers", "SELECT tgname FROM pg_trigger WHERE tgrelid = 'public.articles'::regclass AND NOT tgisinternal ORDER BY 1")
    sql("catalog_version", "SELECT count(*) > 0 FROM hdb_catalog.hdb_source_catalog_version")


INTROSPECTION = """query IntrospectionQuery { __schema { queryType { name } mutationType { name }
 subscriptionType { name } types { ...FullType } directives { name description locations args { ...InputValue } } } }
fragment FullType on __Type { kind name description fields(includeDeprecated: true) { name description
 args { ...InputValue } type { ...TypeRef } isDeprecated deprecationReason } inputFields { ...InputValue }
 interfaces { ...TypeRef } enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason }
 possibleTypes { ...TypeRef } }
fragment InputValue on __InputValue { name description type { ...TypeRef } defaultValue }
fragment TypeRef on __Type { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name
 ofType { kind name ofType { kind name ofType { kind name } } } } } } } }"""

if __name__ == "__main__":
    main()
