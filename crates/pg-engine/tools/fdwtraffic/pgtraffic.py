#!/usr/bin/env python3
"""Write the fdw_traffic workload (tests/fdw_traffic.rs, same phases) as a psql script for
PostgreSQL's postgres_fdw, through the counting proxy on port 53075; then summarize.

usage: pgtraffic.py gen <proxy port> <counters file> [<server cgroup dir>] > work.sql
       pgtraffic.py sum < psql output

PGT_PHASES=<prefix>,<prefix> keeps only the phases whose names start so, PGT_SETUP=0 leaves
the setup out (for profiles: set up once, then profile the phases).

With a cgroup directory (the server container's, as the psql container sees it), each phase
also reports the server's CPU (cpu.stat usage_usec: every process of the server, both ends of
the loopback) and the summary its peak memory (memory.peak).
"""
import os, re, sys

# (gen: the proxy's port and the counters file as the psql container sees it.)
PORT = sys.argv[2] if len(sys.argv) > 2 else "53075"
COUNTERS = sys.argv[3] if len(sys.argv) > 3 else "/traffic/counters"
CGROUP = sys.argv[4] if len(sys.argv) > 4 else None

SETUP = [
    "CREATE EXTENSION IF NOT EXISTS postgres_fdw",
    "CREATE SERVER loop FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '%s', port '%s', dbname 'postgres')" % (".".join(["127", "0", "0", "1"]), PORT),
    "CREATE USER MAPPING FOR CURRENT_USER SERVER loop OPTIONS (user 'postgres', password 'pw')",
    "CREATE TABLE base (id int PRIMARY KEY, v text, n numeric)",
    "INSERT INTO base SELECT g, 'value ' || g, g * 1.5 FROM generate_series(1, 20000) g",
    "CREATE TABLE side (id int PRIMARY KEY, w text)",
    "INSERT INTO side SELECT g, 'w' || g FROM generate_series(1, 2000) g",
    "CREATE FOREIGN TABLE ft (id int, v text, n numeric) SERVER loop OPTIONS (table_name 'base')",
    "CREATE FOREIGN TABLE fs (id int, w text) SERVER loop OPTIONS (table_name 'side')",
    "CREATE TABLE loc (id int)",
    "INSERT INTO loc SELECT g * 7 FROM generate_series(1, 200) g",
    "ANALYZE loc",
    "CREATE TABLE kid (id int, v text, extra int)",
    "INSERT INTO kid SELECT g, 'k' || g, g FROM generate_series(1, 2000) g",
    "CREATE TABLE par (id int, v text)",
    "CREATE FOREIGN TABLE fkid (extra int) INHERITS (par) SERVER loop OPTIONS (table_name 'kid')",
    "CREATE FUNCTION keep() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
    "CREATE TRIGGER keep BEFORE UPDATE ON fkid FOR EACH ROW EXECUTE FUNCTION keep()",
    "CREATE TABLE kid2 (id int, v text, extra int)",
    "INSERT INTO kid2 SELECT g, 'k' || g, g FROM generate_series(1, 2000) g",
    "CREATE TABLE par2 (id int, v text)",
    "CREATE FOREIGN TABLE fkid2 (extra int) INHERITS (par2) SERVER loop OPTIONS (table_name 'kid2')",
]


def phases():
    yield "point lookups", ["SELECT * FROM ft WHERE id = %d" % (i * 37 % 20000 + 1) for i in range(1, 501)]
    yield "lookups in one txn", ["BEGIN"] + ["SELECT v FROM ft WHERE id = %d" % (i * 13 % 20000 + 1) for i in range(1, 301)] + ["COMMIT"]
    yield "full scans (20k rows)", ["SELECT * FROM ft"] * 10
    yield "range scans (~31 rows)", ["SELECT * FROM ft WHERE id BETWEEN %d AND %d" % (i * 50, i * 50 + 30) for i in range(200)]
    yield "foreign joins", ["SELECT ft.v, fs.w FROM ft JOIN fs ON ft.id = fs.id WHERE fs.id < %d" % (10 + i) for i in range(100)]
    yield "remote aggregates", ["SELECT count(*), sum(n) FROM ft WHERE id > %d" % (i * 100) for i in range(100)]
    yield "local join foreign", ["SELECT count(*) FROM loc JOIN ft ON ft.id = loc.id"] * 5
    yield "single-row inserts", ["INSERT INTO ft VALUES (%d, 'x', 1)" % (100000 + i) for i in range(300)]
    yield "direct updates", ["UPDATE ft SET v = 'y' WHERE id = %d" % (100000 + i) for i in range(100)]
    yield "inherited updates, row trigger (50 rows)", ["UPDATE par SET v = v || 'z' WHERE id BETWEEN %d AND %d" % (i * 50, i * 50 + 49) for i in range(20)]
    yield "inherited updates, sent whole (50 rows)", ["UPDATE par2 SET v = v || 'z' WHERE id BETWEEN %d AND %d" % (i * 50, i * 50 + 49) for i in range(20)]
    yield "locked local join foreign", ["SELECT loc.id, ft.v FROM loc JOIN ft ON ft.id = loc.id WHERE loc.id < %d FOR UPDATE OF loc" % (300 + i) for i in range(20)]
    yield "locked local join of a foreign join", ["SELECT loc.id, ft.v, fs.w FROM loc, ft, fs WHERE ft.id = fs.id AND ft.id = loc.id AND fs.id < %d FOR UPDATE OF loc" % (300 + i) for i in range(20)]

    # (Analyzed, so both push the joins below; its own phase, last, so the ones above keep
    # their never-analyzed plans.)
    yield "analyze foreign tables", ["ANALYZE ft, fs"]
    yield "update from foreign join (10 rows)", ["UPDATE ft SET v = ft.v || 'u' FROM fs WHERE ft.id = fs.id AND fs.id BETWEEN %d AND %d" % (i * 20 + 1, i * 20 + 10) for i in range(50)]
    yield "delete using foreign join (3 rows)", ["DELETE FROM ft USING fs WHERE ft.id = fs.id + 99999 AND fs.id BETWEEN %d AND %d" % (i * 3 + 1, i * 3 + 3) for i in range(100)]


def gen():
    print("\\set QUIET on")
    print("SET compute_query_id = off;")
    for q in SETUP if os.environ.get("PGT_SETUP", "1") == "1" else []:
        print(q + ";")
    print("SELECT 1 FROM ft LIMIT 1;")  # (the connection is made before the first phase)
    print("\\o /dev/null")
    only = [x for x in os.environ.get("PGT_PHASES", "").split(",") if x]
    for name, sqls in phases():
        if only and not any(name.startswith(o) for o in only):
            continue
        print("\\o")
        print("\\echo PHASE %s %d" % (name, len(sqls)))
        print(stats())
        print("\\o /dev/null")
        print("\\timing on")
        for q in sqls:
            print(q + ";")
        print("\\timing off")
    print("\\o")
    print("\\echo PHASE end 0")
    print(stats())


def stats():
    if CGROUP:
        return "\\! cat %s; grep usage_usec %s/cpu.stat; cat %s/memory.peak" % (COUNTERS, CGROUP, CGROUP)
    return "\\! cat %s" % COUNTERS


def summarize():
    rows, cur, ms, cpu, peak = [], None, 0.0, [], 0
    for line in sys.stdin:
        line = line.strip()
        m = re.match(r"usage_usec (\d+)$", line)
        if m:
            cpu.append(int(m.group(1)))
            continue
        if re.match(r"\d+$", line):
            peak = int(line)
            continue
        m = re.match(r"PHASE (.*) (\d+)$", line)
        if m:
            cur = (m.group(1), int(m.group(2)))
            continue
        m = re.match(r"writes (\d+) out (\d+) in (\d+) conns (\d+)", line)
        if m and cur:
            rows.append((cur[0], cur[1], [int(x) for x in m.groups()], ms))
            ms = 0.0
            continue
        m = re.match(r"Time: ([\d.]+) ms", line)
        if m:
            ms += float(m.group(1))
    for i, ((name, n, a, _), (_, _, b, t)) in enumerate(zip(rows, rows[1:])):
        d = [y - x for x, y in zip(a, b)]
        c = " cpu %8.1f ms" % ((cpu[i + 1] - cpu[i]) / 1000) if len(cpu) == len(rows) else ""
        print("%-42s stmts %5d writes %6d (%.2f/stmt) out %9d B in %10d B %9.1f ms%s" % (name, n, d[0], d[0] / max(n, 1), d[1], d[2], t, c))
    if len(cpu) == len(rows) and rows:
        print("total %.1f ms, server cpu %.1f ms, server peak memory %.1f MB" % (sum(r[3] for r in rows), (cpu[-1] - cpu[0]) / 1000, peak / 1e6))


if __name__ == "__main__":
    gen() if sys.argv[1] == "gen" else summarize()
