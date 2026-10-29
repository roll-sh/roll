# PgBouncer 1.24.1 (Debian trixie's package, unpacked without root) in front of the server:
# SCRAM authentication through auth_query (pg_shadow read as postgres), transaction pooling
# with protocol-level prepared statements re-prepared on other server connections
# (max_prepared_statements), session pooling with its server_reset_query (DISCARD ALL),
# statement pooling, COPY, cancel requests forwarded through it, server-side cursors, errors,
# and many clients over a pool of two server connections. One id per check,
# `<n>:<check>#<hash>`, the hash over what the client saw (rows, status, SQLSTATE and message),
# so a check answered differently shows as one id per server; the results are in
# $OUT/results.txt. psycopg 3.2.9 (pip) is the client, over pgbouncer's Unix socket.
# Database: pgbouncer.setup.sql (pgb_test, pgb_user).
# image: python:3.13-slim
# src:
set -eu
P=/cache/src/pgbouncer
if [ ! -x $P/root/usr/sbin/pgbouncer ]; then
  rm -rf $P/apt && mkdir -p $P/apt/lists/partial $P/apt/cache/archives/partial $P/root
  A="-qq -o Dir::State::Lists=$P/apt/lists -o Dir::Cache=$P/apt/cache -o Debug::NoLocking=1"
  apt-get $A update >/dev/null
  (cd $P/apt && apt-get $A download pgbouncer=1.24.1-1+deb13u2 libevent-2.1-7t64 libcares2)
  for d in $P/apt/*.deb; do dpkg-deb -x "$d" $P/root; done
fi
export LD_LIBRARY_PATH=$P/root/usr/lib/x86_64-linux-gnu
V=/cache/venv/pgbouncer
if [ ! -x $V/bin/python ]; then
  python -m venv $V
  $V/bin/pip install -q psycopg==3.2.9 psycopg-binary==3.2.9
fi
W=/tmp/pgb && mkdir -p $W
cat > $W/userlist.txt <<EOF
"postgres" "pw"
EOF
cat > $W/pgbouncer.ini <<EOF
[databases]
pgb_tx = host=$PGHOST port=$PGPORT dbname=pgb_test pool_mode=transaction
pgb_session = host=$PGHOST port=$PGPORT dbname=pgb_test pool_mode=session
pgb_stmt = host=$PGHOST port=$PGPORT dbname=pgb_test pool_mode=statement
[pgbouncer]
listen_addr =
unix_socket_dir = $W
listen_port = 6432
auth_type = scram-sha-256
auth_file = $W/userlist.txt
auth_user = postgres
default_pool_size = 2
max_client_conn = 100
max_prepared_statements = 50
server_reset_query = DISCARD ALL
ignore_startup_parameters = extra_float_digits,options
logfile = $W/pgbouncer.log
pidfile = $W/pgbouncer.pid
EOF
$P/root/usr/sbin/pgbouncer -d $W/pgbouncer.ini
sleep 1
$V/bin/python - <<'PY'
import hashlib, json, os, threading, time
import psycopg

OUT = os.environ["OUT"]
W = "/tmp/pgb"


def conn(db="pgb_tx", **kw):
    return psycopg.connect(host=W, port=6432, dbname=db, user="pgb_user", password="pgbpw",
                           connect_timeout=10, **kw)


def shown(v):
    if isinstance(v, (list, tuple)):
        return [shown(x) for x in v]
    if isinstance(v, (int, float, str, bool)) or v is None:
        return v
    return str(v)


def run(fn):
    try:
        return {"ok": shown(fn())}
    except psycopg.Error as e:
        d = e.diag
        return {"error": [d.sqlstate, d.message_primary, d.message_detail]}
    except Exception as e:  # noqa: BLE001
        return {"exc": type(e).__name__, "msg": str(e)[:200]}


def q(c, sql, params=None, prepare=None):
    with c.cursor() as cur:
        cur.execute(sql, params, prepare=prepare)
        return cur.fetchall() if cur.description else cur.statusmessage


checks = []


def check(name):
    def deco(fn):
        checks.append((name, fn))
        return fn
    return deco


@check("auth_query_scram")
def _():
    with conn() as c:
        return q(c, "SELECT current_user, session_user, current_database()")


@check("wrong_password")
def _():
    psycopg.connect(host=W, port=6432, dbname="pgb_tx", user="pgb_user", password="nope").close()


@check("simple_rows")
def _():
    with conn() as c:
        return q(c, "SELECT id, name, qty, tags FROM items WHERE id <= 5 ORDER BY id")


@check("prepared_across_server_connections")
def _():
    # Three clients over two server connections, each statement prepared (named, protocol
    # level) and run in many transactions: PgBouncer prepares it again on whichever server
    # connection a transaction gets.
    out = []
    cs = [conn(prepare_threshold=0) for _ in range(3)]
    try:
        for i in range(12):
            c = cs[i % 3]
            with c.transaction():
                out.append(q(c, "SELECT name, qty FROM items WHERE id = %s", (i + 1,), prepare=True))
                out.append(q(c, "SELECT count(*) FROM items WHERE qty > %s", (i * 100,), prepare=True))
    finally:
        for c in cs:
            c.close()
    return out


@check("prepared_autocommit")
def _():
    with conn(prepare_threshold=0, autocommit=True) as c:
        return [q(c, "SELECT %s::int + %s::int, %s::text", (i, i, f"x{i}"), prepare=True) for i in range(8)]


@check("transaction_isolation")
def _():
    with conn() as c, conn() as d:
        with c.transaction():
            q(c, "UPDATE counter SET n = n + 1 WHERE id = 1")
            seen = q(d, "SELECT n FROM counter WHERE id = 1")
        after = q(d, "SELECT n FROM counter WHERE id = 1")
        return [seen, after]


@check("set_local_in_transaction")
def _():
    with conn() as c:
        with c.transaction():
            q(c, "SET LOCAL statement_timeout = '1234ms'")
            inside = q(c, "SHOW statement_timeout")
        return [inside]


@check("rollback_error")
def _():
    with conn() as c:
        try:
            with c.transaction():
                q(c, "INSERT INTO items (id, name) VALUES (1, 'dup')")
        except psycopg.Error as e:
            first = [e.diag.sqlstate, e.diag.message_primary, e.diag.message_detail, e.diag.constraint_name]
        return [first, q(c, "SELECT count(*) FROM items")]


@check("syntax_error_position")
def _():
    with conn() as c:
        try:
            q(c, "SELEC 1")
        except psycopg.Error as e:
            return [e.diag.sqlstate, e.diag.message_primary, e.diag.statement_position]


@check("copy_in_out")
def _():
    with conn() as c:
        with c.transaction():
            q(c, "CREATE TEMP TABLE t_copy (a int, b text) ON COMMIT DROP")
            with c.cursor().copy("COPY t_copy FROM STDIN") as cp:
                for i in range(100):
                    cp.write_row((i, f"row {i}\tx"))
            rows = []
            with c.cursor().copy("COPY (SELECT * FROM t_copy WHERE a % 25 = 0 ORDER BY a) TO STDOUT") as cp:
                for r in cp.rows():
                    rows.append(r)
            return [q(c, "SELECT count(*), sum(a) FROM t_copy"), rows]


@check("copy_binary")
def _():
    with conn() as c:
        with c.cursor().copy("COPY (SELECT id, name, tags FROM items WHERE id < 4 ORDER BY id) TO STDOUT (FORMAT BINARY)") as cp:
            cp.set_types(["int4", "text", "text[]"])
            return [r for r in cp.rows()]


@check("cancel_through_pooler")
def _():
    with conn() as c:
        t = threading.Timer(0.5, c.cancel)
        t.start()
        try:
            q(c, "SELECT slow(5)")
            return "not cancelled"
        except psycopg.Error as e:
            return [e.diag.sqlstate, e.diag.message_primary]
        finally:
            t.cancel()


@check("statement_timeout")
def _():
    with conn() as c:
        with c.transaction():
            q(c, "SET LOCAL statement_timeout = 200")
            try:
                q(c, "SELECT slow(2)")
            except psycopg.Error as e:
                return [e.diag.sqlstate, e.diag.message_primary]


@check("server_cursor_in_transaction")
def _():
    with conn() as c:
        with c.transaction():
            with c.cursor(name="cur1") as cur:
                cur.itersize = 7
                cur.execute("SELECT id FROM items WHERE id % 10 = 0 ORDER BY id")
                return [r[0] for r in cur]


@check("session_reset")
def _():
    # Session pooling: a temporary table and a setting are gone for the next client of the
    # same server connection (server_reset_query DISCARD ALL).
    with conn("pgb_session") as c:
        q(c, "CREATE TEMP TABLE t_sess (a int)")
        q(c, "SET application_name = 'leaky'")
        q(c, "PREPARE p1 AS SELECT 1")
        c.commit()
    with conn("pgb_session") as c:
        return [q(c, "SELECT count(*) FROM pg_class WHERE relname = 't_sess'"),
                q(c, "SELECT count(*) FROM pg_prepared_statements"),
                run(lambda: q(c, "EXECUTE p1"))]


@check("session_listen_notify")
def _():
    with conn("pgb_session", autocommit=True) as c, conn(autocommit=True) as d:
        q(c, "LISTEN pgb_chan")
        q(d, "NOTIFY pgb_chan, 'hello'")
        q(d, "SELECT pg_notify('pgb_chan', 'again')")
        got = []
        for n in c.notifies(timeout=2, stop_after=2):
            got.append([n.channel, n.payload])
        return got


@check("statement_mode_refuses_transactions")
def _():
    with conn("pgb_stmt", autocommit=True) as c:
        a = q(c, "SELECT 1")
        b = run(lambda: q(c, "BEGIN"))
        return [a, b]


@check("many_clients_small_pool")
def _():
    errors = []

    def work(k):
        try:
            with conn(prepare_threshold=0) as c:
                for _ in range(20):
                    with c.transaction():
                        q(c, "UPDATE counter SET n = n + 1 WHERE id = 1", prepare=True)
        except Exception as e:  # noqa: BLE001
            errors.append(str(e)[:100])

    with conn() as c:
        before = q(c, "SELECT n FROM counter WHERE id = 1")[0][0]
    ts = [threading.Thread(target=work, args=(k,)) for k in range(8)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    with conn() as c:
        after = q(c, "SELECT n FROM counter WHERE id = 1")[0][0]
    return [after - before, errors]


@check("deadlock_free_row_locks")
def _():
    with conn() as c, conn() as d:
        with c.transaction():
            q(c, "SELECT * FROM items WHERE id = 7 FOR UPDATE")
            r = run(lambda: q(d, "SELECT id FROM items WHERE id = 7 FOR UPDATE NOWAIT"))
        return r


@check("types_roundtrip")
def _():
    with conn(prepare_threshold=0) as c:
        return q(c, "SELECT %s::numeric, %s::timestamptz, %s::jsonb, %s::bytea, %s::uuid, %s::int[]",
                 ("12345678901234567890.123", "2025-01-02 03:04:05+00", json.dumps({"a": [1, None]}),
                  b"\x00\x01binary", "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", [1, 2, None]), prepare=True)


@check("large_result")
def _():
    with conn() as c:
        rows = q(c, "SELECT g, repeat('x', g % 100) FROM generate_series(1, 20000) g")
        return [len(rows), rows[-1][0], sum(len(r[1]) for r in rows)]


@check("parameter_status")
def _():
    with conn() as c:
        p = c.info
        return [p.parameter_status("server_version"), p.parameter_status("client_encoding"),
                p.parameter_status("DateStyle"), p.parameter_status("integer_datetimes"),
                p.parameter_status("standard_conforming_strings")]


with open(f"{OUT}/results.txt", "w") as rep, open(f"{OUT}/pgbouncer.lst", "w") as lst:
    for n, (name, fn) in enumerate(checks):
        res = json.dumps(run(fn), sort_keys=True)
        cid = f"{n:02d}:{name}"
        h = hashlib.sha1(res.encode()).hexdigest()[:10]
        rep.write(f"== {cid}\n{res}\n")
        lst.write(f"PASS {cid}#{h}\n")
PY
kill "$(cat $W/pgbouncer.pid)" 2>/dev/null || true
tail -5 $W/pgbouncer.log
