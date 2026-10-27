"""The driver suite for Python's `cassandra-driver` and `scylla-driver` (the same module,
`cassandra`). `crates/nosql-server/tests/drivers.rs` runs it in Docker against roll and against
the reference images; every suite (python, java, go, node) has the same checks under the same
names, and prints one line per check: `PASS <name>`, `FAIL <name>: <why>` or `SKIP <name>: <why>`.

Environment: HOST, PORT, DIALECT (scylla|cassandra), DC (expected datacenter), USER / PASS
(PasswordAuthenticator), FEATURES=1 (UDFs and materialized views enabled), SHARDS (expected
shard count, ScyllaDB), RESTART=1 (the harness restarts the server when it reads `@@RESTART`
and answers on stdin), KS (keyspace to use; dropped at the end).
"""

import datetime
import decimal
import ipaddress
import os
import sys
import time
import traceback
import uuid

from cassandra import AuthenticationFailed, ConsistencyLevel
from cassandra.auth import PlainTextAuthProvider
from cassandra.cluster import EXEC_PROFILE_DEFAULT, Cluster, ExecutionProfile, NoHostAvailable
from cassandra.policies import DCAwareRoundRobinPolicy, TokenAwarePolicy
from cassandra.query import BatchStatement, BatchType, SimpleStatement
from cassandra.util import Date, Duration, Time

HOST = os.environ.get("HOST", "127.0.0.1")
PORT = int(os.environ.get("PORT", "9042"))
DIALECT = os.environ.get("DIALECT", "scylla")
DC = os.environ.get("DC", "datacenter1")
USER = os.environ.get("USER_NAME") or None
PASS = os.environ.get("PASS") or None
FEATURES = os.environ.get("FEATURES") == "1"
SHARDS = int(os.environ.get("SHARDS", "0") or 0)
RESTART = os.environ.get("RESTART") == "1"
KS = os.environ.get("KS", "drv_python")

failed = 0


def out(line):
    print(line, flush=True)


def check(name, fn, *args):
    global failed
    try:
        r = fn(*args)
        if isinstance(r, str) and r.startswith("SKIP"):
            out(f"SKIP {name}: {r[5:]}")
        else:
            out(f"PASS {name}")
    except Exception as e:  # noqa: BLE001 - every failure is reported, the suite goes on
        failed += 1
        msg = f"{type(e).__name__}: {e}".replace("\n", " ")[:400]
        out(f"FAIL {name}: {msg}")
        if os.environ.get("VERBOSE"):
            traceback.print_exc()


def expect(cond, msg):
    if not cond:
        raise AssertionError(msg)


def eventually(fn, timeout=15.0, step=0.2):
    end = time.time() + timeout
    while True:
        try:
            r = fn()
            if r:
                return r
        except Exception:  # noqa: BLE001
            if time.time() > end:
                raise
        if time.time() > end:
            raise AssertionError("timed out")
        time.sleep(step)


def cluster(**kw):
    auth = PlainTextAuthProvider(USER, PASS) if USER else None
    profile = ExecutionProfile(load_balancing_policy=TokenAwarePolicy(DCAwareRoundRobinPolicy(local_dc=DC)), request_timeout=30)
    return Cluster([HOST], port=PORT, auth_provider=auth, execution_profiles={EXEC_PROFILE_DEFAULT: profile},
                   connect_timeout=10, schema_event_refresh_window=0.2, **kw)


c = cluster()
s = None


def connect():
    global s
    s = c.connect()
    expect(c.protocol_version >= 4, f"protocol v{c.protocol_version}")


check("connect", connect)
if s is None:
    sys.exit(1)


def bad_password():
    if not USER:
        return "SKIP no authenticator"
    bad = Cluster([HOST], port=PORT, auth_provider=PlainTextAuthProvider(USER, "wrong-password"), connect_timeout=10)
    try:
        bad.connect()
        raise AssertionError("connected with a wrong password")
    except NoHostAvailable as e:
        errs = list(e.errors.values())
        expect(errs and isinstance(errs[0], AuthenticationFailed), f"errors {errs}")
    finally:
        bad.shutdown()


check("auth.bad_password", bad_password)


def system_local():
    r = s.execute("SELECT release_version, data_center, rack, cluster_name FROM system.local").one()
    want = "3.0.8" if DIALECT == "scylla" else "5.0.9"
    expect(r.release_version == want, f"release_version {r.release_version}")
    expect(r.data_center == DC, f"data_center {r.data_center}")
    host = c.metadata.all_hosts()[0]
    expect(host.datacenter == DC and host.rack == r.rack, f"host {host.datacenter}/{host.rack}")


check("system.local", system_local)

s.execute(f"DROP KEYSPACE IF EXISTS {KS}")
s.execute(f"CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', '{DC}': 1}}")
s.set_keyspace(KS)
s.execute("CREATE TYPE addr (street text, zip int, tags frozen<set<text>>)")
s.execute("CREATE TABLE kv (k int, c int, v text, PRIMARY KEY (k, c)) WITH CLUSTERING ORDER BY (c DESC) AND comment = 'drivers'")
s.execute("CREATE TABLE counters (k int PRIMARY KEY, n counter)")
s.execute("CREATE TABLE lwt (k int PRIMARY KEY, v text, n int)")
s.execute("CREATE INDEX kv_v ON kv (v)")

# (check label, CQL type, value); the column is `c_<label>`. Every suite uses these labels.
TYPES = [
    ("ascii", "ascii", "plain ascii"),
    ("bigint", "bigint", -9223372036854775808),
    ("blob", "blob", b"\x00\x01\xfe\xff"),
    ("boolean", "boolean", True),
    ("date", "date", Date(datetime.date(2024, 2, 29))),
    ("decimal", "decimal", decimal.Decimal("-1234567890.0987654321")),
    ("double", "double", 1.5e300),
    ("duration", "duration", Duration(1, 2, 3_000_000_004)),
    ("float", "float", 0.5),
    ("inet4", "inet", "192.168.1.10"),
    ("inet6", "inet", "2001:db8::1"),
    ("int", "int", -2147483648),
    ("smallint", "smallint", -32768),
    ("text", "text", "texte unicode \u00e9\u4e2d\U0001f600"),
    ("time", "time", Time(datetime.time(23, 59, 59, 999999))),
    ("timestamp", "timestamp", datetime.datetime(2026, 10, 20, 8, 0, 1, 234000)),
    ("timeuuid", "timeuuid", uuid.UUID("d2177dd0-eaa2-11de-a572-001b779c76e3")),
    ("tinyint", "tinyint", -128),
    ("uuid", "uuid", uuid.UUID("01234567-89ab-cdef-0123-456789abcdef")),
    ("varchar", "varchar", "varchar"),
    ("varint", "varint", 123456789012345678901234567890),
    ("list", "list<int>", [3, 1, 2]),
    ("set", "set<text>", {"b", "a"}),
    ("map", "map<text, int>", {"x": 1, "y": 2}),
    ("nested", "frozen<list<frozen<map<int, text>>>>", [{1: "one"}, {2: "two"}]),
    ("tuple", "tuple<int, text, double>", (1, "t", 2.5)),
    ("udt", "frozen<addr>", None),  # an Addr, below
    ("udt_nonfrozen", "addr", None),
    ("vector", "vector<float, 3>", [0.5, 1.0, -2.0]),
]
cols = ", ".join(f"c_{n} {t}" for n, t, _ in TYPES)
s.execute(f"CREATE TABLE types (k int PRIMARY KEY, {cols})")


class Addr:
    def __init__(self, street=None, zip=None, tags=None):
        self.street, self.zip, self.tags = street, zip, tags

    def __eq__(self, o):
        return (self.street, self.zip, set(self.tags or ())) == (o.street, o.zip, set(o.tags or ()))

    def __repr__(self):
        return f"Addr({self.street!r}, {self.zip!r}, {self.tags!r})"


c.register_user_type(KS, "addr", Addr)


def metadata():
    c.refresh_schema_metadata()
    ks = c.metadata.keyspaces[KS]
    expect(ks.replication_strategy.__class__.__name__ == "NetworkTopologyStrategy", f"strategy {ks.replication_strategy}")
    t = ks.tables["kv"]
    expect([x.name for x in t.partition_key] == ["k"] and [x.name for x in t.clustering_key] == ["c"], "keys")
    expect(t.columns["c"].cql_type == "int" and t.columns["v"].cql_type == "text", "column types")
    expect(t.clustering_key[0].is_reversed, "clustering order DESC")
    expect(t.options.get("comment") == "drivers", f"options {t.options.get('comment')}")
    tt = ks.tables["types"]
    for n, ty, _ in TYPES:
        # varchar is an alias the schema stores as text; tuples are always frozen.
        ty = "text" if ty == "varchar" else f"frozen<{ty}>" if ty.startswith("tuple") else ty
        expect(tt.columns[f"c_{n}"].cql_type == ty, f"{n}: {tt.columns[f'c_{n}'].cql_type} != {ty}")
    ddl = t.export_as_string()
    expect("CREATE TABLE" in ddl and "CLUSTERING ORDER BY (c DESC)" in ddl, ddl[:200])


check("metadata.table", metadata)


def metadata_udt():
    u = c.metadata.keyspaces[KS].user_types["addr"]
    expect(u.field_names == ["street", "zip", "tags"], f"fields {u.field_names}")
    expect(u.field_types == ["text", "int", "frozen<set<text>>"], f"types {u.field_types}")


check("metadata.udt", metadata_udt)


def metadata_index():
    idx = c.metadata.keyspaces[KS].indexes["kv_v"]
    expect(idx.table_name == "kv" and idx.kind == "COMPOSITES", f"{idx.table_name} {idx.kind}")
    expect(idx.index_options.get("target") == "v", f"options {idx.index_options}")


check("metadata.index", metadata_index)


def metadata_view():
    if not FEATURES and DIALECT == "cassandra":
        return "SKIP materialized views are disabled"
    s.execute("CREATE MATERIALIZED VIEW kv_by_v AS SELECT k, c, v FROM kv WHERE v IS NOT NULL AND k IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, k, c)")
    c.refresh_schema_metadata()
    v = c.metadata.keyspaces[KS].views["kv_by_v"]
    expect(v.base_table_name == "kv", v.base_table_name)
    expect([x.name for x in v.partition_key] == ["v"] and [x.name for x in v.clustering_key] == ["k", "c"], "view keys")


check("metadata.view", metadata_view)


def metadata_functions():
    if not FEATURES:
        return "SKIP UDFs are disabled"
    if DIALECT == "scylla":
        s.execute("CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x * 2'")
        s.execute("CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return (acc or 0) + (x or 0)'")
    else:
        s.execute("CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x * 2;'")
        s.execute("CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return (acc == null ? 0 : acc) + (x == null ? 0 : x);'")
    s.execute("CREATE AGGREGATE total(int) SFUNC plus STYPE int INITCOND 0")
    c.refresh_schema_metadata()
    ks = c.metadata.keyspaces[KS]
    f = ks.functions["twice(int)"]
    expect(f.argument_names == ["x"] and f.return_type == "int" and f.language == ("lua" if DIALECT == "scylla" else "java"), f"function {f.argument_names} {f.return_type} {f.language}")
    a = ks.aggregates["total(int)"]
    expect(a.state_func == "plus" and a.state_type == "int" and a.initial_condition == "0", f"aggregate {a.state_func} {a.state_type} {a.initial_condition}")


check("metadata.functions", metadata_functions)


def schema_agreement():
    wait = getattr(s, "wait_for_schema_agreement", None) or c.control_connection.wait_for_schema_agreement
    expect(wait(wait_time=10), "no agreement")
    s.execute("CREATE TABLE agree (k int PRIMARY KEY)")
    expect(wait(wait_time=10), "no agreement after DDL")


check("schema_agreement", schema_agreement)


def prepared():
    ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)")
    expect([x[2] for x in ins.column_metadata] == ["k", "c", "v"], "bind metadata")
    expect(ins.routing_key_indexes == [0], f"routing key {ins.routing_key_indexes}")
    for i in range(3):
        s.execute(ins, (1, i, f"v{i}"))
    sel = s.prepare("SELECT c, v FROM kv WHERE k = :k AND c >= :lo")
    rows = list(s.execute(sel, {"k": 1, "lo": 1}))
    expect([(r.c, r.v) for r in rows] == [(2, "v2"), (1, "v1")], f"rows {rows}")
    expect([x[2] for x in sel.result_metadata] == ["c", "v"], "result metadata")


check("prepared", prepared)


def paging():
    ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (2, ?, 'p')")
    for i in range(250):
        s.execute(ins, (i,))
    st = SimpleStatement("SELECT c FROM kv WHERE k = 2", fetch_size=50)
    rs = s.execute(st)
    expect(len(rs.current_rows) == 50 and rs.has_more_pages, f"first page {len(rs.current_rows)}")
    first = [r.c for r in rs.current_rows]
    rs2 = s.execute(st, paging_state=rs.paging_state)
    expect([r.c for r in rs2.current_rows][0] == first[-1] - 1, "resumed page")
    allc = [r.c for r in s.execute(st)]
    expect(allc == list(range(249, -1, -1)), f"{len(allc)} rows")


check("paging", paging)


def batches():
    b = BatchStatement(batch_type=BatchType.LOGGED)
    ins = s.prepare("INSERT INTO kv (k, c, v) VALUES (?, ?, ?)")
    b.add(ins, (3, 1, "b1"))
    b.add(SimpleStatement("INSERT INTO kv (k, c, v) VALUES (4, 1, 'b2')"))
    s.execute(b)
    ub = BatchStatement(batch_type=BatchType.UNLOGGED)
    ub.add(ins, (3, 2, "u1"))
    ub.add(ins, (3, 3, "u2"))
    s.execute(ub)
    cb = BatchStatement(batch_type=BatchType.COUNTER)
    up = s.prepare("UPDATE counters SET n = n + ? WHERE k = ?")
    cb.add(up, (5, 1))
    cb.add(up, (2, 1))
    s.execute(cb)
    expect(len(list(s.execute("SELECT * FROM kv WHERE k = 3"))) == 3, "logged+unlogged rows")
    expect(s.execute("SELECT n FROM counters WHERE k = 1").one().n == 7, "counter")


check("batches", batches)


def lwt():
    ins = s.prepare("INSERT INTO lwt (k, v, n) VALUES (?, ?, ?) IF NOT EXISTS")
    r = s.execute(ins, (1, "first", 1))
    expect(r.was_applied, "first insert applied")
    r = s.execute(ins, (1, "second", 2))
    expect(not r.was_applied and r.one().v == "first", f"second insert {r.one()}")
    up = s.prepare("UPDATE lwt SET v = ? WHERE k = ? IF n = ?")
    expect(s.execute(up, ("third", 1, 1)).was_applied, "conditional update")
    expect(not s.execute(up, ("fourth", 1, 5)).was_applied, "failed condition")
    if hasattr(ins, "is_lwt") and DIALECT == "scylla":  # ScyllaDB's protocol extension
        expect(ins.is_lwt() and up.is_lwt(), "prepared LWT marking")
        expect(not s.prepare("SELECT v FROM lwt WHERE k = ?").is_lwt(), "non-LWT marked")


check("lwt", lwt)

for idx, (label, ty, value) in enumerate(TYPES):
    if label.startswith("udt"):
        value = Addr("Main St", 12345, {"home", "work"})

    def roundtrip(col=f"c_{label}", label=label, value=value, idx=idx):
        s.execute(s.prepare(f"INSERT INTO types (k, {col}) VALUES (?, ?)"), (idx, value))
        got = getattr(s.execute(s.prepare(f"SELECT {col} FROM types WHERE k = ?"), (idx,)).one(), col)
        want = value
        if isinstance(want, set):
            got = set(got)
        if isinstance(want, dict):
            got = dict(got)
        if label == "nested":
            got = [dict(x) for x in got]
        if label.startswith("inet"):
            got, want = ipaddress.ip_address(got), ipaddress.ip_address(want)
        if label == "vector":
            got = list(got)
        expect(got == want, f"{got!r} != {want!r}")

    check(f"types.{label}", roundtrip)


def token_aware():
    sel = s.prepare("SELECT token(k) AS t FROM kv WHERE k = ?")
    for k in (1, 42, -7, 123456):
        bound = sel.bind((k,))
        tok = c.metadata.token_map.token_class.from_key(bound.routing_key)
        server = s.execute(bound).one()
        if server is None:
            s.execute("INSERT INTO kv (k, c, v) VALUES (%s, 0, 't')", (k,))
            server = s.execute(bound).one()
        expect(tok.value == server.t, f"k={k}: driver {tok.value} server {server.t}")
        replicas = c.metadata.get_replicas(KS, bound.routing_key)
        expect(len(replicas) == 1, f"replicas {replicas}")


check("token_aware", token_aware)


def shard_aware():
    host = c.metadata.all_hosts()[0]
    info = getattr(host, "sharding_info", None)
    if not hasattr(host, "sharding_info"):
        return "SKIP not a shard-aware driver"
    if DIALECT != "scylla":
        expect(info is None, "Cassandra has no shards")
        return None
    expect(info is not None and info.shards_count == SHARDS, f"sharding info {info and info.shards_count}")
    expect(info.shard_aware_port == 19042, f"shard-aware port {info.shard_aware_port}")
    pool = s._pools[host]
    eventually(lambda: len(pool._connections) == SHARDS)
    expect(sorted(pool._connections) == list(range(SHARDS)), f"shards {sorted(pool._connections)}")
    # Requests for a key go to the connection of the key's shard.
    for k in range(20):
        tok = c.metadata.token_map.token_class.from_key(s.prepare("SELECT v FROM kv WHERE k = ?").bind((k,)).routing_key)
        shard = info.shard_id_from_token(tok.value)
        expect(0 <= shard < SHARDS, f"shard {shard}")


check("shard_aware", shard_aware)


def tracing():
    rs = s.execute(SimpleStatement("SELECT * FROM kv WHERE k = 1"), trace=True)
    tr = rs.get_query_trace(max_wait_sec=10)
    expect(tr.request_type and tr.duration is not None and tr.coordinator, f"trace {tr.request_type} {tr.duration}")
    expect(len(tr.events) > 0, "no trace events")


check("tracing", tracing)


def events():
    c2 = cluster()
    s2 = c2.connect()
    try:
        s2.execute(f"CREATE TABLE {KS}.evt (k int PRIMARY KEY, v text)")
        eventually(lambda: "evt" in c.metadata.keyspaces[KS].tables)
        s2.execute(f"ALTER TABLE {KS}.evt ADD w int")
        eventually(lambda: "w" in c.metadata.keyspaces[KS].tables["evt"].columns)
        s2.execute(f"DROP TABLE {KS}.evt")
        eventually(lambda: "evt" not in c.metadata.keyspaces[KS].tables)
    finally:
        c2.shutdown()


check("events.schema", events)


def reconnect():
    if not RESTART:
        return "SKIP the harness doesn't restart this server"
    sel = s.prepare("SELECT v FROM kv WHERE k = ? AND c = ?")
    expect(s.execute(sel, (1, 1)).one().v == "v1", "before restart")
    out("@@RESTART")
    sys.stdin.readline()
    r = eventually(lambda: s.execute(sel, (1, 1)).one(), timeout=60, step=0.5)
    expect(r.v == "v1", "after restart (prepared statement re-prepared, data durable)")


check("reconnect", reconnect)

try:
    s.execute(f"DROP KEYSPACE IF EXISTS {KS}")
except Exception:  # noqa: BLE001
    pass
c.shutdown()
out(f"DONE {failed} failed")
sys.exit(1 if failed else 0)
