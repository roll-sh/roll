"""Grafana against the server: its own database (migrations, then the API's objects) and a
PostgreSQL datasource over grafana_ds (health, the query editor's metadata queries, table and
time-series queries with macros, annotations, variables, errors). See grafana.sh."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import apps  # noqa: E402
import psycopg  # noqa: E402

PORT = int(os.environ.get("APP_PORT", "52450"))
PGHOST, PGPORT = os.environ["PGHOST"], os.environ["PGPORT"]
g = apps.Http("http://127.0.0.1:%d" % PORT, "admin", "admin")
T0, T1 = "1767225600000", "1767247200000"  # 2026-01-01 00:00 .. 06:00 UTC

DS = None


def pick(*keys):
    return lambda b: {k: b.get(k) for k in keys} if isinstance(b, dict) else b


def q(name, sql, fmt="table"):
    body = {
        "queries": [{"refId": "A", "datasource": {"uid": DS, "type": "grafana-postgresql-datasource"},
                     "rawSql": sql, "format": fmt, "rawQuery": True, "editorMode": "code"}],
        "from": T0,
        "to": T1,
    }
    g.step("ds:" + name, "POST", "/api/ds/query", body)


def main():
    t = time.time()
    if not apps.wait_http("http://127.0.0.1:%d/api/health" % PORT, 900):
        apps.record("startup", "grafana never answered", ok=False)
        return
    apps.record("startup", "ok")
    print("grafana up after %.1fs" % (time.time() - t), flush=True)

    app = psycopg.connect(host=PGHOST, port=PGPORT, user="postgres", password="pw", dbname="grafana_app")
    app.autocommit = True
    apps.db_rows(app, "migrations", "SELECT count(*), count(*) FILTER (WHERE success), "
                 "count(*) FILTER (WHERE error <> '') FROM migration_log")
    apps.db_rows(app, "migration_errors", "SELECT migration_id, error FROM migration_log "
                 "WHERE NOT success OR error <> '' ORDER BY migration_id")
    apps.schema_digest(app)

    global DS
    st, ds = g.step("ds:create", "POST", "/api/datasources", {
        "name": "pg", "type": "grafana-postgresql-datasource", "access": "proxy",
        "url": "%s:%s" % (PGHOST, PGPORT), "user": "postgres", "database": "grafana_ds",
        "jsonData": {"sslmode": "disable", "postgresVersion": 1800, "timescaledb": False, "database": "grafana_ds"},
        "secureJsonData": {"password": "pw"},
    }, keep=pick("message", "name"))
    DS = (ds.get("datasource") or {}).get("uid") if isinstance(ds, dict) else None
    g.step("ds:health", "GET", "/api/datasources/uid/%s/health" % DS)
    # The query editor's metadata (postgresMetaQuery / the SQL builder) and version probes.
    q("version", "SELECT current_setting('server_version_num')::int/100 as version")
    q("timescale", "SELECT 1 FROM pg_extension WHERE extname='timescaledb'")
    q("schemas", "SELECT quote_ident(schema_name) AS schema FROM information_schema.schemata "
      "WHERE schema_name NOT IN ('information_schema') AND schema_name NOT LIKE 'pg_%' ORDER BY 1")
    q("tables", "select quote_ident(table_name) as \"table\" from information_schema.tables where "
      "quote_ident(table_schema) not in ('information_schema','pg_catalog','_timescaledb_cache',"
      "'_timescaledb_catalog','_timescaledb_internal','_timescaledb_config','timescaledb_information',"
      "'timescaledb_experimental') and table_type = 'BASE TABLE' and quote_ident(table_schema) IN "
      "(select CASE WHEN trim(s[i]) = '\"$user\"' THEN user ELSE trim(s[i]) END from "
      "generate_series(array_lower(string_to_array(current_setting('search_path'),','),1),"
      "array_upper(string_to_array(current_setting('search_path'),','),1)) as i, "
      "string_to_array(current_setting('search_path'),',') s) order by 1")
    q("columns", "select quote_ident(column_name) as \"column\", data_type as \"type\" from "
      "information_schema.columns where quote_ident(table_name) = 'metrics' order by ordinal_position")
    q("timecolumns", "SELECT quote_ident(column_name) AS column_name, data_type FROM information_schema.columns "
      "WHERE table_name = 'metrics' AND data_type IN ('timestamp without time zone','timestamp with time zone',"
      "'bigint','integer','double precision','real') ORDER BY 1")
    # Queries a dashboard runs.
    q("table", "SELECT host, count(*), round(avg(cpu)::numeric, 3) AS cpu, max(mem), sum(load) "
      "FROM metrics WHERE $__timeFilter(ts) GROUP BY host ORDER BY host")
    q("series", "SELECT $__timeGroupAlias(ts, '30m'), host AS metric, avg(cpu) AS value FROM metrics "
      "WHERE $__timeFilter(ts) GROUP BY 1, 2 ORDER BY 1, 2", "time_series")
    q("series_fill", "SELECT $__timeGroupAlias(ts, '20m', NULL), max(load) AS load FROM metrics "
      "WHERE $__timeFilter(ts) AND host = 'db-1' GROUP BY 1 ORDER BY 1", "time_series")
    q("hourly", "SELECT $__timeGroup(ts, '1h') AS time, count(*) AS n, $__timeFrom() <= min(ts) AS inside "
      "FROM metrics WHERE ts < $__timeTo() GROUP BY 1 ORDER BY 1", "time_series")
    q("wide", "SELECT ts AS time, cpu, mem, load FROM metrics WHERE host = 'web-1' AND $__timeFilter(ts) "
      "ORDER BY ts LIMIT 50", "time_series")
    q("view", "SELECT * FROM host_avg ORDER BY host")
    q("types", "SELECT 1::int2 a, 2::int8 b, 1.5::float4 c, 2.25::numeric d, true e, 'x'::char(3) f, "
      "'2026-01-01'::date g, '12:30'::time h, '1 day'::interval i, '{1,2}'::int[] j, '{\"a\":1}'::jsonb k, "
      "gen_random_uuid() IS NOT NULL l, 'warn'::level m, '\\x0102'::bytea n, '10.0.0.1'::inet o")
    q("schema_table", "SELECT * FROM reports.daily ORDER BY day")
    q("annotations", "SELECT at AS time, ends AS timeend, title AS text, tags FROM events "
      "WHERE $__timeFilter(at) ORDER BY at")
    q("variable", "SELECT DISTINCT host AS __text, host AS __value FROM metrics ORDER BY 1")
    q("error_syntax", "SELEC 1")
    q("error_column", "SELECT nope FROM metrics")
    q("error_div", "SELECT 1/0")
    q("error_type", "SELECT 'x'::int")
    q("multi", "SELECT 1 AS a; SELECT 2 AS b")
    q("empty", "SELECT * FROM metrics WHERE false")
    q("nulls", "SELECT NULL::int AS n, NULL::text AS t, NULL::timestamptz AS ts")
    # Grafana's own objects: folders, dashboards, versions, search, stars.
    g.step("folder:create", "POST", "/api/folders", {"title": "Ops"}, keep=pick("title"))
    st, f = g.call("GET", "/api/folders")
    fuid = f[0]["uid"] if isinstance(f, list) and f else None
    dash = {"title": "Hosts", "tags": ["ops", "pg"], "panels": [
        {"type": "timeseries", "title": "CPU", "gridPos": {"x": 0, "y": 0, "w": 12, "h": 8},
         "targets": [{"refId": "A", "rawSql": "SELECT 1", "format": "time_series"}]}],
        "schemaVersion": 39}
    st, d = g.step("dash:create", "POST", "/api/dashboards/db", {"dashboard": dash, "folderUid": fuid},
                   keep=pick("status"))
    duid = d.get("uid") if isinstance(d, dict) else None
    for i in range(3):
        dash2 = dict(dash, uid=duid, panels=dash["panels"] * (i + 2))
        g.step("dash:update%d" % i, "POST", "/api/dashboards/db",
               {"dashboard": dash2, "folderUid": fuid, "overwrite": True, "message": "v%d" % i},
               keep=pick("status"))
    g.step("dash:get", "GET", "/api/dashboards/uid/%s" % duid,
           keep=lambda b: b.get("dashboard", {}).get("panels") if isinstance(b, dict) else b)
    g.step("dash:versions", "GET", "/api/dashboards/uid/%s/versions" % duid,
           keep=lambda b: [v.get("message") for v in (b.get("versions") if isinstance(b, dict) else b) or []]
           if isinstance(b, (list, dict)) else b)
    g.step("search", "GET", "/api/search?query=Hos&tag=ops")
    g.step("search:all", "GET", "/api/search?type=dash-db&sort=alpha-asc")
    g.step("search:tags", "GET", "/api/dashboards/tags")
    g.step("star", "POST", "/api/user/stars/dashboard/uid/%s" % duid)
    g.step("stars", "GET", "/api/user/stars", keep=lambda b: len(b) if isinstance(b, list) else b)
    # Users, teams, orgs, preferences, service accounts.
    g.step("user:create", "POST", "/api/admin/users",
           {"name": "Ann", "email": "ann@example.com", "login": "ann", "password": "secret-pass-1"},
           keep=pick("message"))
    g.step("users", "GET", "/api/users?perpage=10&page=1")
    g.step("users:search", "GET", "/api/users/search?query=an")
    g.step("team:create", "POST", "/api/teams", {"name": "sre", "email": "sre@example.com"}, keep=pick("message"))
    st, ts = g.call("GET", "/api/teams/search?name=sre")
    tid = ts["teams"][0]["id"] if isinstance(ts, dict) and ts.get("teams") else 0
    st, us = g.call("GET", "/api/users/lookup?loginOrEmail=ann")
    g.step("team:member", "POST", "/api/teams/%s/members" % tid,
           {"userId": us.get("id") if isinstance(us, dict) else 0})
    g.step("team:members", "GET", "/api/teams/%s/members" % tid,
           keep=lambda b: [m.get("login") for m in b] if isinstance(b, list) else b)
    g.step("org:create", "POST", "/api/orgs", {"name": "Second"}, keep=pick("message"))
    g.step("orgs", "GET", "/api/orgs")
    g.step("prefs", "PUT", "/api/user/preferences", {"theme": "dark", "timezone": "utc", "weekStart": "monday"})
    g.step("prefs:get", "GET", "/api/user/preferences")
    g.step("sa:create", "POST", "/api/serviceaccounts", {"name": "ci", "role": "Viewer"}, keep=pick("name", "role"))
    g.step("sa:search", "GET", "/api/serviceaccounts/search?query=ci",
           keep=lambda b: [(s.get("name"), s.get("role")) for s in b.get("serviceAccounts", [])]
           if isinstance(b, dict) else b)
    # Annotations, playlists, library panels, short URLs, snapshots, query history.
    for i, txt in enumerate(["first", "second", "third"]):
        g.step("annotation:%d" % i, "POST", "/api/annotations",
               {"time": int(T0) + i * 600000, "timeEnd": int(T0) + i * 600000 + 60000, "tags": ["a", "t%d" % i],
                "text": txt}, keep=pick("message"))
    g.step("annotations", "GET", "/api/annotations?from=%s&to=%s&tags=a&limit=10" % (T0, T1),
           keep=lambda b: sorted(a.get("text") for a in b) if isinstance(b, list) else b)
    g.step("annotation:tags", "GET", "/api/annotations/tags?tag=t")
    g.step("playlist", "POST", "/api/playlists", {"name": "wall", "interval": "5m",
                                                  "items": [{"type": "dashboard_by_tag", "value": "ops"}]},
           keep=pick("name", "interval"))
    g.step("playlists", "GET", "/api/playlists")
    g.step("libpanel", "POST", "/api/library-elements",
           {"name": "cpu panel", "kind": 1, "model": {"type": "stat", "title": "cpu"}, "folderUid": fuid},
           keep=lambda b: (b.get("result") or {}).get("name") if isinstance(b, dict) else b)
    g.step("libpanels", "GET", "/api/library-elements?searchString=cpu",
           keep=lambda b: [e.get("name") for e in (b.get("result") or {}).get("elements", [])]
           if isinstance(b, dict) else b)
    g.step("shorturl", "POST", "/api/short-urls", {"path": "d/abc/hosts?orgId=1"},
           keep=lambda b: sorted(b) if isinstance(b, dict) else b)
    g.step("snapshot", "POST", "/api/snapshots", {"dashboard": dash, "expires": 3600},
           keep=lambda b: sorted(b) if isinstance(b, dict) else b)
    g.step("snapshots", "GET", "/api/dashboard/snapshots",
           keep=lambda b: [s.get("name") for s in b] if isinstance(b, list) else b)
    g.step("qhistory", "POST", "/api/query-history",
           {"datasourceUid": DS, "queries": [{"refId": "A", "rawSql": "SELECT 1"}]},
           keep=lambda b: (b.get("result") or {}).get("queries") if isinstance(b, dict) else b)
    g.step("qhistory:search", "GET", "/api/query-history?datasourceUid=%s" % DS,
           keep=lambda b: len(((b or {}).get("result") or {}).get("queryHistory") or []) if isinstance(b, dict) else b)
    # Alerting: a rule over the datasource, its evaluation, the rules' list.
    g.step("alert:folder", "POST", "/api/folders", {"title": "Alerts"}, keep=pick("title"))
    st, af = g.call("GET", "/api/folders")
    auid = ([x["uid"] for x in af if x.get("title") == "Alerts"] or [""])[0] if isinstance(af, list) else ""
    rule = {"title": "high cpu", "ruleGroup": "g1", "folderUID": auid, "noDataState": "OK",
            "execErrState": "Error", "for": "1m", "orgID": 1, "condition": "C",
            "data": [
                {"refId": "A", "relativeTimeRange": {"from": 600, "to": 0}, "datasourceUid": DS,
                 "model": {"refId": "A", "rawSql": "SELECT now() AS time, max(cpu) AS value FROM metrics",
                           "format": "time_series", "rawQuery": True}},
                {"refId": "C", "datasourceUid": "__expr__",
                 "model": {"refId": "C", "type": "threshold", "expression": "A",
                           "conditions": [{"evaluator": {"type": "gt", "params": [10]}}]}}]}
    g.step("alert:rule", "POST", "/api/v1/provisioning/alert-rules", rule, keep=pick("title", "condition", "message"))
    g.step("alert:rules", "GET", "/api/v1/provisioning/alert-rules",
           keep=lambda b: [r.get("title") for r in b] if isinstance(b, list) else b)
    g.step("alert:eval", "POST", "/api/v1/eval", {"data": rule["data"]},
           keep=lambda b: {k: v.get("status") for k, v in (b.get("results") or {}).items()}
           if isinstance(b, dict) else b)
    g.step("alert:contact", "POST", "/api/v1/provisioning/contact-points",
           {"name": "ops-mail", "type": "email", "settings": {"addresses": "ops@example.com"}},
           keep=pick("name", "type"))
    # Deletes, and what is left.
    g.step("dash:delete", "DELETE", "/api/dashboards/uid/%s" % duid, keep=pick("title"))
    g.step("search:after", "GET", "/api/search?type=dash-db")
    apps.db_rows(app, "rows:dashboard", "SELECT title, is_folder FROM dashboard ORDER BY title, is_folder")
    apps.db_rows(app, "rows:user", "SELECT login, email, is_admin FROM \"user\" ORDER BY login")
    apps.db_rows(app, "rows:annotation", "SELECT text, epoch, epoch_end FROM annotation ORDER BY epoch")
    apps.db_rows(app, "rows:versions", "SELECT count(*) FROM dashboard_version")


if __name__ == "__main__":
    main()
