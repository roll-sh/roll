#!/usr/bin/env python3
"""The size models' mixed workload (BENCH.md "Size models, round 2"), scaled by N = mw1's rows:
pages per fork (main / fsm / vm) and four EXPLAINs before and after a clean restart, 300
inserts into each table with their ctids, VACUUM of both, the report again, 500 more inserts
and an UPDATE of every 97th row with their ctids, the report again. Run it against PostgreSQL
18.6 and against the engine and diff the two outputs: every line must match.

  mw.py pg N OUT --server CONTAINER --port P --psql HELPER
      (a reference container on the host network serving on P; restarted with docker restart)
  mw.py engine N OUT --bin ROLL_ENGINE --data DIR --port P --psql HELPER
      (starts the engine on 127.0.0.1:P with a fresh DIR, restarts it cleanly)

HELPER is a container of the pinned PostgreSQL image on the host network (`--entrypoint sleep
<image> infinity`) whose psql reaches both. STATS=1 adds pgstatindex and reltuples to the
report (the reference needs pgstattuple). The engine's log goes to DIR.log. ENGINE_ARGS adds
engine arguments, e.g. "-c autovacuum=off" for a reference started with it.
"""
import argparse, atexit, hashlib, os, shutil, signal, subprocess, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("mode", choices=["pg", "engine"])
ap.add_argument("n", type=int)
ap.add_argument("out")
ap.add_argument("--port", type=int, required=True)
ap.add_argument("--psql", required=True)
ap.add_argument("--server")
ap.add_argument("--bin")
ap.add_argument("--data")
a = ap.parse_args()
out = open(a.out, "w")
stats = bool(os.environ.get("STATS"))


def psql(sql, db="postgres", check=True):
    cmd = ["docker", "exec", "-i", "-e", "PGPASSWORD=pw", a.psql, "psql", "-X", "-q", "-At",
           "-v", "ON_ERROR_STOP=1", "-h", "127.0.0.1", "-p", str(a.port), "-U", "postgres", db]
    r = subprocess.run(cmd, input=sql, capture_output=True, text=True)
    if r.returncode != 0:
        if not check:
            return None
        sys.exit("psql failed: " + r.stderr[-2000:])
    return r.stdout


def wait_up():
    for _ in range(1200):
        if psql("select 1", check=False) is not None:
            return
        time.sleep(0.5)
    sys.exit("server did not start")


eng = None


def start_engine(fresh):
    global eng
    if fresh:
        shutil.rmtree(a.data, ignore_errors=True)
    log = open(a.data + ".log", "a")
    eng = subprocess.Popen(["nice", "-n", "19", a.bin, "--data", a.data, "--listen", f"127.0.0.1:{a.port}",
                            "--password", "pw", "--idle-close-secs", "600"] + os.environ.get("ENGINE_ARGS", "").split(),
                           stdout=log, stderr=log)
    wait_up()


def stop_engine():
    global eng
    if eng is not None and eng.poll() is None:
        eng.send_signal(signal.SIGTERM)
        eng.wait(timeout=600)
    eng = None


atexit.register(stop_engine)


def restart():
    if a.mode == "pg":
        subprocess.run(["docker", "restart", a.server], check=True, capture_output=True)
        wait_up()
    else:
        stop_engine()
        start_engine(False)


n = a.n
m2 = n * 2 // 3
load = f"""
SET synchronous_commit = off;
CREATE TABLE mw1 (id int PRIMARY KEY, k int, v text, pad char(40)); CREATE INDEX mw1_k ON mw1 (k);
CREATE INDEX mw1_v ON mw1 (v); CREATE TABLE mw2 (id bigserial PRIMARY KEY, ref int, note text);
CREATE INDEX mw2_ref ON mw2 (ref);
INSERT INTO mw1 SELECT g, g % 1000, md5(g::text), 'p' FROM generate_series(1, {n}) g;
INSERT INTO mw2 (ref, note) SELECT g % 5000, repeat('n', g % 40) FROM generate_series(1, {m2}) g;
UPDATE mw1 SET k = k + 1 WHERE id % 7 = 0; UPDATE mw1 SET pad = 'q' WHERE id % 5 = 0;
UPDATE mw1 SET v = md5(v) WHERE id % 11 = 0; DELETE FROM mw1 WHERE id % 13 = 0;
DELETE FROM mw2 WHERE id % 3 = 0; VACUUM mw2;
INSERT INTO mw1 SELECT g, g % 50, md5((g * 3)::text), 'r' FROM generate_series({n + 1}, {n + n // 6}) g;
INSERT INTO mw2 (ref, note) SELECT g % 700, 'again' FROM generate_series(1, {n * 2 // 15}) g;
UPDATE mw2 SET note = note || '!' WHERE ref % 9 = 0; DELETE FROM mw1 WHERE id BETWEEN {n // 3} AND {n // 3 + n // 10};
"""
rels = ["mw1", "mw1_pkey", "mw1_k", "mw1_v", "mw2", "mw2_pkey", "mw2_ref"]
report = "SELECT " + " || ' ' || ".join(
    f"'{r}=' || pg_relation_size('{r}')/8192 || '/' || pg_relation_size('{r}','fsm')/8192"
    f" || '/' || pg_relation_size('{r}','vm')/8192" for r in rels) + ";\n"
report += "EXPLAIN SELECT * FROM mw1 WHERE k = 5;\nEXPLAIN SELECT count(*) FROM mw2 WHERE ref < 100;\n"
if stats:
    report += ("SELECT * FROM pgstatindex('mw1_pkey');\n"
               "SELECT relname, reltuples FROM pg_class WHERE relname LIKE 'mw%' ORDER BY 1;\n")
report += "EXPLAIN SELECT * FROM mw1 WHERE v > 'f';\nEXPLAIN SELECT * FROM mw1 WHERE id < 1000;\n"


def ctids(table, sql):
    rows = psql(sql + " RETURNING ctid", "mw").split()
    first, last = (rows[0], rows[-1]) if rows else ("", "")
    return f"{table} {len(rows)} {first}..{last} md5={hashlib.md5(' '.join(rows).encode()).hexdigest()[:10]}\n"


if a.mode == "engine":
    start_engine(True)
else:
    wait_up()
t0 = time.time()
psql("DROP DATABASE IF EXISTS mw;")
psql("CREATE DATABASE mw;")
psql(load, "mw")
if stats:
    psql("CREATE EXTENSION pgstattuple", "mw")
print(f"load {time.time() - t0:.1f}s", file=sys.stderr)
out.write("before\n" + psql(report, "mw"))
restart()
out.write("after\n" + psql(report, "mw"))
big = n + n // 6
out.write(ctids("mw1", f"INSERT INTO mw1 SELECT g, g % 77, md5(g::text), 's' FROM generate_series({big + 1}, {big + 300}) g"))
out.write(ctids("mw2", "INSERT INTO mw2 (ref, note) SELECT g, 'post' FROM generate_series(1, 300) g"))
if stats:
    out.write("inserted\n" + psql(report, "mw"))
psql("VACUUM mw1; VACUUM mw2;", "mw")
out.write("vacuumed\n" + psql(report, "mw"))
out.write(ctids("mw1", f"INSERT INTO mw1 SELECT g, g % 77, md5(g::text), 't' FROM generate_series({big + 301}, {big + 800}) g"))
out.write(ctids("mw1u", "UPDATE mw1 SET pad = 'u' WHERE id % 97 = 0"))
out.write("final\n" + psql(report, "mw"))
psql("DROP DATABASE mw;")
stop_engine()
print(f"done {time.time() - t0:.1f}s", file=sys.stderr)
