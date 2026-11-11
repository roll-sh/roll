"""The queries monitoring tools send, run against one server: pgwatch's metrics, pg_activity's,
pganalyze-collector's, Datadog's postgres integration's and check_postgres's (each tool's
source at a pinned version, its SQL extracted from it). Per query one id
`<tool>/<name>#<hash>`, the hash over what the client saw: the SQLSTATE of an error, else the
column names and types, each column's NULLs (none / some / all) and whether rows came back
(0, 1, more). A query a server answers differently shows as one id per server. Details
(the signature in clear) go to $OUT/details.tsv, median times of three runs to
$OUT/timings.tsv. Queries the source's templates make unrunnable (a syntax error on both
servers alike) still get an id: they compare equal."""
import hashlib, io, json, os, pathlib, re, statistics, tarfile, time, urllib.request

import psycopg
import yaml

SRC = pathlib.Path(os.environ["SRC"])
OUT = pathlib.Path(os.environ["OUT"])
SOURCES = {
    "pgwatch": "https://raw.githubusercontent.com/cybertec-postgresql/pgwatch/v3.7.0/internal/metrics/metrics.yaml",
    "pg_activity": "https://github.com/dalibo/pg_activity/archive/refs/tags/v3.6.1.tar.gz",
    "pganalyze": "https://github.com/pganalyze/collector/archive/refs/tags/v0.66.1.tar.gz",
    # (check_postgres's 2.26.0 is its master: the tag doesn't carry the script.)
    "check_postgres": "https://raw.githubusercontent.com/bucardo/check_postgres/master/check_postgres.pl",
}
DATADOG = "https://raw.githubusercontent.com/DataDog/integrations-core/postgres-22.12.0/postgres/datadog_checks/postgres/"
DATADOG_FILES = ["util.py", "relationsmanager.py", "postgres.py", "statements.py", "statement_samples.py", "metadata.py"]


def fetch(tool):
    d = SRC / tool
    if not d.exists() and tool == "datadog":
        d.mkdir(parents=True)
        for f in DATADOG_FILES:
            (d / f).write_bytes(urllib.request.urlopen(DATADOG + f, timeout=300).read())
    if not d.exists():
        d.mkdir(parents=True)
        data = urllib.request.urlopen(SOURCES[tool], timeout=300).read()
        url = SOURCES[tool]
        if url.endswith(".tar.gz"):
            keep = {"pg_activity": "/pgactivity/queries/", "pganalyze": "/input/postgres/"}[tool]
            with tarfile.open(fileobj=io.BytesIO(data)) as t:
                for m in t.getmembers():
                    if keep in m.name and m.isfile() and not m.name.endswith("_test.go"):
                        (d / m.name.replace("/", "__")).write_bytes(t.extractfile(m).read())
        else:
            (d / url.rsplit("/", 1)[1]).write_bytes(data)
    return d


def pgwatch():
    doc = yaml.safe_load((fetch("pgwatch") / "metrics.yaml").read_text())
    for name, m in sorted(doc["metrics"].items()):
        sqls = {int(k): v for k, v in (m.get("sqls") or {}).items()}
        ok = [v for v in sqls if v <= 18]
        if ok:
            yield name, sqls[max(ok)], m.get("init_sql")


def pg_activity():
    best = {}
    for p in sorted(fetch("pg_activity").glob("*.sql")):
        base = p.name.split("__")[-1][:-4]
        m = re.match(r"(.*)_post_(\d+)$", base)
        name, ver = (m.group(1), int(m.group(2))) if m else (re.sub(r"_oldest$", "", base), 0)
        if ver <= 180000 and ver >= best.get(name, (-1, ""))[0]:
            best[name] = (ver, p.read_text())
    params = {"dbname_filter": None, "min_duration": 0, "pid": 0, "prev_total_size": 0, "skip_db_size": False, "using_rds": False}
    for name, (_, sql) in sorted(best.items()):
        if not name.startswith(("do_", "disable_", "reset_")):
            yield name, sql, {"params": params}


def fill(sql):
    """A template's placeholders, filled with what makes the query run as the tool sends it
    in its common case (no extra filter, no extra column)."""
    sql = re.sub(r"(WHERE|AND|OR)\s+%[sdv]", r"\1 true", sql, flags=re.I)
    sql = re.sub(r",\s*%[sdv](?=\s*(,|FROM|\n\s*FROM))", "", sql, flags=re.I)
    sql = re.sub(r"%[sdv]", "", sql)
    sql = re.sub(r"\{[a-z_]*filter[a-z_]*\}", "true", sql, flags=re.I)
    sql = re.sub(r"\{[^{}]*\}", "", sql)
    return sql


def pganalyze():
    for p in sorted(fetch("pganalyze").glob("*.go")):
        src = p.read_text()
        for m in re.finditer(r"(?:const|var)?\s*(\w+)\s*(?:string)?\s*=\s*`([^`]*)`", src):
            if re.match(r"\s*(SELECT|WITH)\b", m.group(2), re.I):
                yield p.name.split("__")[-1][:-3] + "." + m.group(1), fill(m.group(2)), None


def datadog():
    for p in sorted(fetch("datadog").glob("*.py")):
        src = p.read_text()
        for m in re.finditer(r"(\w+)\s*=\s*(?:\{[^{}]*?'query':\s*)?[rf]?(\"\"\"|''')(.*?)\2", src, re.S):
            if re.match(r"\s*(SELECT|WITH)\b", m.group(3), re.I):
                yield p.name.split("__")[-1][:-3] + "." + m.group(1), fill(m.group(3)), None


def check_postgres():
    src = (fetch("check_postgres") / "check_postgres.pl").read_text(errors="replace")
    for i, m in enumerate(re.finditer(r"\$SQL\s*=\s*q\{(.*?)\};", src, re.S)):
        sql = m.group(1)
        if re.match(r"\s*(SELECT|WITH)\b", sql, re.I) and "$" not in sql:
            yield f"q{i:03d}", sql, None


def signature(cur, sql, params):
    try:
        cur.execute(sql, params, prepare=False) if params is not None else cur.execute(sql.replace("%", "%%"), prepare=False)
        rows = cur.fetchall() if cur.description else []
    except psycopg.Error as e:
        return {"error": e.sqlstate}
    cols = [(d.name, d.type_code) for d in cur.description or []]
    nulls = []
    for i in range(len(cols)):
        n = sum(r[i] is None for r in rows)
        nulls.append("all" if rows and n == len(rows) else "some" if n else "none")
    return {"cols": cols, "nulls": nulls, "rows": min(len(rows), 2)}


def main():
    conn = psycopg.connect(autocommit=True)
    cur = conn.cursor()
    ids, details, timings = [], [], []
    for tool, gen in [("pgwatch", pgwatch), ("pg_activity", pg_activity), ("pganalyze", pganalyze), ("datadog", datadog), ("check_postgres", check_postgres)]:
        for name, sql, extra in gen():
            # (pgwatch's helper functions, which its init_sql makes first.)
            params = extra.get("params") if isinstance(extra, dict) else None
            if isinstance(extra, str):
                try:
                    cur.execute(extra.replace("%", "%%"), prepare=False)
                except psycopg.Error:
                    pass
            cur.execute("SET statement_timeout = '30s'")
            times, sig = [], None
            for _ in range(3):
                t = time.perf_counter()
                sig = signature(cur, sql, params)
                times.append((time.perf_counter() - t) * 1000)
            text = json.dumps(sig, sort_keys=True)
            h = hashlib.sha1(text.encode()).hexdigest()[:8]
            ids.append(f"PASS {tool}/{name}#{h}")
            details.append(f"{tool}/{name}\t{text}")
            timings.append(f"{tool}/{name}\t{statistics.median(times):.2f}")
    (OUT / "results.lst").write_text("\n".join(ids) + "\n")
    (OUT / "details.tsv").write_text("\n".join(details) + "\n")
    (OUT / "timings.tsv").write_text("\n".join(timings) + "\n")
    print(len(ids), "queries")


main()
