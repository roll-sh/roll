"""Shared driver for the application suites (grafana, metabase, hasura, keycloak, gitea):
HTTP requests against an application whose own database is the server under test, each
answer reduced to a stable form and recorded as one id, `<step>#<hash>`, so an answer that
differs between the engine and PostgreSQL shows as one id per server. The answers themselves
go to $OUT/responses.txt (and the ids to $OUT/<suite>.lst as PASS lines), for diffing.

Volatile fields (ids, uids, timestamps, versions, tokens) are dropped before hashing; what is
left is what the application computed from the database: rows, columns, error messages.
"""
import base64
import hashlib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

OUT = os.environ.get("OUT", "/tmp")
SUITE = os.environ.get("SUITE_NAME", "app")
_lst = None
_resp = None
VOLATILE = {
    "id", "uid", "orgId", "created", "updated", "createdAt", "updatedAt", "created_at", "updated_at",
    "version", "url", "key", "token", "lastSeenAt", "lastSeenAtAge", "avatarUrl", "secureJsonFields",
    "dashboardId", "folderId", "folderUid", "userId", "teamId", "datasourceId", "ruleId", "timestamp",
    "time", "expires", "expiresAt", "folderUrl", "traceID", "traceId", "iat", "exp", "sessionId", "slug", "meta", "duration",
}
TS = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:?\d\d)?")


def _open():
    global _lst, _resp
    if _lst is None:
        _lst = open(os.path.join(OUT, SUITE + ".lst"), "w")
        _resp = open(os.path.join(OUT, "responses.txt"), "w")


def norm(v, drop=VOLATILE):
    """`v` without volatile keys (`drop`: a set of names, or a predicate), timestamps masked,
    dicts in key order."""
    if isinstance(v, dict):
        gone = drop if callable(drop) else drop.__contains__
        return {k: norm(x, drop) for k, x in sorted(v.items()) if not gone(k)}
    if isinstance(v, list):
        return [norm(x, drop) for x in v]
    if isinstance(v, str):
        return TS.sub("<ts>", v)
    return v


def record(step, value, ok=True):
    """One id for `step`: its normalized value's hash."""
    _open()
    text = json.dumps(value, sort_keys=True, default=str)
    h = hashlib.sha256(text.encode()).hexdigest()[:12]
    _lst.write("%s %s#%s\n" % ("PASS" if ok else "FAIL", step, h))
    _resp.write("== %s #%s\n%s\n" % (step, h, text[:4000]))
    _lst.flush()
    _resp.flush()


class Http:
    def __init__(self, base, user=None, password=None, headers=None):
        self.base = base.rstrip("/")
        self.headers = dict(headers or {})
        if user is not None:
            tok = base64.b64encode(("%s:%s" % (user, password)).encode()).decode()
            self.headers["Authorization"] = "Basic " + tok

    def call(self, method, path, body=None, headers=None, form=None, raw=False, timeout=120):
        h = dict(self.headers)
        h.update(headers or {})
        data = None
        if form is not None:
            data = urllib.parse.urlencode(form).encode()
            h["Content-Type"] = "application/x-www-form-urlencoded"
        elif body is not None:
            data = json.dumps(body).encode()
            h["Content-Type"] = "application/json"
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                status, payload = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, payload = e.code, e.read()
        except Exception as e:  # connection refused, timeouts
            return 0, {"error": type(e).__name__}
        if raw:
            return status, payload
        try:
            return status, json.loads(payload or b"null")
        except ValueError:
            return status, payload.decode(errors="replace")[:2000]

    def step(self, name, method, path, body=None, drop=VOLATILE, keep=None, **kw):
        """Call and record: the status and the normalized body (or `keep(body)`)."""
        status, out = self.call(method, path, body, **kw)
        shown = keep(out) if keep else out
        record(name, {"status": status, "body": norm(shown, drop)}, ok=0 < status < 500)
        return status, out


def wait_http(url, seconds=600, ok=lambda s: s == 200):
    end = time.time() + seconds
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=5) as r:
                if ok(r.status):
                    return True
        except urllib.error.HTTPError as e:
            if ok(e.code):
                return True
        except Exception:
            pass
        time.sleep(1)
    return False


def schema_digest(conn, schemas=("public",)):
    """The application's tables as its migrations left them: one record per table (columns
    with types, nullability and defaults; indexes; constraints), through a DB-API connection."""
    cur = conn.cursor()
    cur.execute(
        """SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default,
                  character_maximum_length, numeric_precision
           FROM information_schema.columns WHERE table_schema = ANY(%s)
           ORDER BY 1, 2, ordinal_position""",
        (list(schemas),),
    )
    tables = {}
    for r in cur.fetchall():
        tables.setdefault("%s.%s" % (r[0], r[1]), {"columns": [], "indexes": [], "constraints": []})["columns"].append(
            [r[2], r[3], r[4], r[5], r[6], r[7]]
        )
    cur.execute(
        "SELECT schemaname, tablename, indexdef FROM pg_indexes WHERE schemaname = ANY(%s) ORDER BY 1, 2, 3",
        (list(schemas),),
    )
    for s, t, d in cur.fetchall():
        tables.setdefault("%s.%s" % (s, t), {"columns": [], "indexes": [], "constraints": []})["indexes"].append(d)
    cur.execute(
        """SELECT n.nspname, c.relname, x.contype, pg_get_constraintdef(x.oid)
           FROM pg_constraint x JOIN pg_class c ON c.oid = x.conrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = ANY(%s) ORDER BY 1, 2, 3, 4""",
        (list(schemas),),
    )
    for s, t, k, d in cur.fetchall():
        tables.setdefault("%s.%s" % (s, t), {"columns": [], "indexes": [], "constraints": []})["constraints"].append(
            [k, d]
        )
    # (Tables named with a random suffix: Metabase's search_index__<random>.)
    rnd = re.compile(r"(search_index__)\w+")
    tables = {rnd.sub(r"\1*", n): {k: [rnd.sub(r"\1*", str(x)) for x in v] for k, v in t.items()}
              for n, t in tables.items()}
    for name, t in sorted(tables.items()):
        record("schema:" + name, t)
    record("schema:tables", sorted(tables))
    return tables


def db_rows(conn, step, sql, args=()):
    cur = conn.cursor()
    try:
        cur.execute(sql, args)
        rows = [list(r) for r in cur.fetchall()] if cur.description else cur.rowcount
        record(step, norm(rows))
    except Exception as e:  # the server's error, as the driver reports it
        conn.rollback()
        record(step, {"error": str(e).split("\n")[0]})
        return None
    return rows


if __name__ == "__main__":
    print("a library", file=sys.stderr)
