# Cassandra's tombstone warning (`tombstone_warn_threshold`, 1000): tables with ~1100 tombstones
# per partition of several kinds (row, cell and range deletions, static rows, a 600-character
# key, a DESC clustering column) and the warnings of each page of many query shapes. Writes JSON
# lines (page size, query, per page: rows and warnings), turned into Rust by
# crates/nosql-engine/scripts/gen_tombstones.py (src/tests/captured_tombstones.rs, whose `setup`
# must stay in step with `setup` here).
#
# Runs inside the Cassandra reference container with the Python driver cqlsh bundles:
#   docker exec -i nosql-ref-cassandra python3 - setup < tombstones.py | docker exec -i nosql-ref-cassandra cqlsh
#   docker exec -i nosql-ref-cassandra python3 - probe < tombstones.py > ~/.cache/roll-nosql/capture/out/tombstones.jsonl
#   docker exec nosql-ref-cassandra cqlsh -e "DROP KEYSPACE gap_"
# ScyllaDB answers the same reads without warnings.
import sys, glob, json


def setup():
    L = "x" * 600
    lines = [
        "CREATE KEYSPACE IF NOT EXISTS gap_ WITH replication = {'class':'SimpleStrategy','replication_factor':1};",
        "CREATE TABLE gap_.t (k int, c int, v int, s int static, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.u (\"Key\" text, k2 int, c1 int, c2 text, v text, w int, PRIMARY KEY ((\"Key\", k2), c1, c2)) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC);",
        "CREATE TABLE gap_.w (k int, c int, a int, b int, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.r (k int, c int, v int, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.p (k int PRIMARY KEY, v int, m map<text, int>, l list<int>);",
        "CREATE TABLE gap_.x (k bigint, c text, d uuid, b blob, v int, PRIMARY KEY (k, c, d));",
        "CREATE TABLE gap_.lng (k text, c int, v int, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.st (k int, c int, s int static, v int, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.f (k int, c int, v int, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.big (k int, c int, v int, PRIMARY KEY (k, c));",
        "CREATE TABLE gap_.col (k int, c int, s set<int>, m map<text, int>, v int, PRIMARY KEY (k, c));",
    ]
    a = lines.append
    for c in range(1100):
        a(f"DELETE FROM gap_.t WHERE k = 1 AND c = {c};")
        a(f"DELETE FROM gap_.t WHERE k = 3 AND c = {c};")
    for c in range(2000, 2005):
        a(f"INSERT INTO gap_.t (k, c, v) VALUES (1, {c}, {c});")
    for c in range(3):
        a(f"INSERT INTO gap_.t (k, c, v) VALUES (2, {c}, {c});")
    a("INSERT INTO gap_.t (k, c, v) VALUES (3, 2000, 1);")
    for c in range(1100):
        a(f"DELETE FROM gap_.u WHERE \"Key\" = 'it''s' AND k2 = 7 AND c1 = {c} AND c2 = 'x';")
    a("INSERT INTO gap_.u (\"Key\", k2, c1, c2, v, w) VALUES ('it''s', 7, 5000, 'a', 'hello', 1);")
    for c in range(1100):
        a(f"INSERT INTO gap_.w (k, c, a, b) VALUES (1, {c}, 1, 1);")
        a(f"DELETE a FROM gap_.w WHERE k = 1 AND c = {c};")
    for c in range(600):
        a(f"DELETE FROM gap_.r WHERE k = 1 AND c > {c * 10} AND c < {c * 10 + 5};")
    a("INSERT INTO gap_.r (k, c, v) VALUES (1, 100000, 1);")
    for k in range(1100):
        a(f"DELETE v FROM gap_.p WHERE k = {k};")
    a("INSERT INTO gap_.p (k, v) VALUES (5000, 1);")
    for i in range(1100):
        a(f"DELETE FROM gap_.x WHERE k = 9 AND c = 'a''b' AND d = 00000000-0000-0000-0000-{i:012d};")
    a("INSERT INTO gap_.x (k, c, d, b, v) VALUES (9, 'zz', 00000000-0000-0000-0000-000000000001, 0xcafe, 1);")
    for c in range(1100):
        a(f"DELETE FROM gap_.lng WHERE k = '{L}' AND c = {c};")
    a(f"INSERT INTO gap_.lng (k, c, v) VALUES ('{L}', 5000, 1);")
    a("INSERT INTO gap_.st (k, s) VALUES (1, 7);")
    a("INSERT INTO gap_.st (k, s) VALUES (2, 8);")
    for c in range(1100):
        a(f"DELETE FROM gap_.st WHERE k = 1 AND c = {c};")
    # Live rows and tombstones interleaved, then 1200 tombstones and a live row.
    for c in range(40):
        if c % 2 == 0:
            a(f"INSERT INTO gap_.f (k, c, v) VALUES (1, {c}, {c});")
        else:
            a(f"DELETE FROM gap_.f WHERE k = 1 AND c = {c};")
    for c in range(100, 1300):
        a(f"DELETE FROM gap_.f WHERE k = 1 AND c = {c};")
    a("INSERT INTO gap_.f (k, c, v) VALUES (1, 2000, 7);")
    # The last partition of a scan holds only tombstones.
    for c in range(1100):
        a(f"DELETE FROM gap_.f WHERE k = 3 AND c = {c};")
    for c in range(10005):
        a(f"INSERT INTO gap_.big (k, c, v) VALUES (1, {c}, 1);")
    for c in range(20000, 21100):
        a(f"DELETE FROM gap_.big WHERE k = 1 AND c = {c};")
    a("INSERT INTO gap_.big (k, c, v) VALUES (1, 30000, 1);")
    for c in range(1100):
        a(f"DELETE FROM gap_.col WHERE k = 1 AND c = {c};")
    a("INSERT INTO gap_.col (k, c, s, m, v) VALUES (1, 5000, {1, 2}, {'a': 1}, 3);")
    print("\n".join(lines))


# "<page size or -> <query>"; unpaged IN queries that warn for several partitions are left out
# (Cassandra reads those partitions concurrently and warns in completion order).
QUERIES = [
    "100 SELECT * FROM gap_.t WHERE k = 1",
    "- SELECT * FROM gap_.t WHERE k = 1",
    "2 SELECT * FROM gap_.t WHERE k = 1",
    "100 SELECT * FROM gap_.t WHERE k IN (1, 3)",
    "3 SELECT * FROM gap_.t WHERE k IN (1, 3)",
    "2 SELECT * FROM gap_.t",
    "- SELECT * FROM gap_.t",
    "- SELECT * FROM gap_.t LIMIT 6",
    "- SELECT count(*) FROM gap_.t",
    "- SELECT max(v) FROM gap_.t WHERE k = 1",
    "- SELECT * FROM gap_.w WHERE k = 1",
    "100 SELECT * FROM gap_.w WHERE k = 1",
    "- SELECT * FROM gap_.r WHERE k = 1",
    "- SELECT * FROM gap_.p",
    "- SELECT k FROM gap_.p",
    "- SELECT m['a'], l FROM gap_.p",
    "- SELECT * FROM gap_.x WHERE k = 9",
    "- SELECT * FROM gap_.x WHERE k = 9 AND c = 'a''b'",
    "- SELECT * FROM gap_.x WHERE k = 9 AND c = 'a''b' AND d > 00000000-0000-0000-0000-000000000005",
    "- SELECT * FROM gap_.x WHERE k = 9 AND c IN ('a''b', 'zz')",
    "- SELECT * FROM gap_.x WHERE k = 9 AND (c, d) > ('a', 00000000-0000-0000-0000-000000000005)",
    "- SELECT * FROM gap_.x WHERE k = 9 AND b = 0xcafe ALLOW FILTERING",
    "- SELECT * FROM gap_.t WHERE k = 1 AND c IN (1, 2, 3, 2000)",
    "- SELECT * FROM gap_.t WHERE k = 1 AND c > 1 AND c <= 3000 ORDER BY c DESC",
    "- SELECT * FROM gap_.u WHERE \"Key\" = 'it''s' AND k2 = 7 ORDER BY c1 ASC",
    "- SELECT * FROM gap_.u WHERE \"Key\" = 'it''s' AND k2 = 7 AND c1 > 3 ORDER BY c1 ASC",
    "- SELECT * FROM gap_.t WHERE k = 1 GROUP BY k, c",
    "- SELECT DISTINCT k FROM gap_.t",
    "- SELECT * FROM gap_.t WHERE token(k) >= token(1)",
    "- SELECT * FROM gap_.t WHERE token(k) > -5000000000000000000 AND token(k) < 0",
    "- SELECT JSON * FROM gap_.t WHERE k = 1",
    "- SELECT v AS vv, writetime(v) FROM gap_.t WHERE k = 1",
    "- SELECT * FROM gap_.t WHERE c = 2000 ALLOW FILTERING",
    "- SELECT * FROM gap_.t WHERE k = 1 AND v > 2001 AND v < 2004 ALLOW FILTERING",
    "- SELECT * FROM gap_.t WHERE k = 1 PER PARTITION LIMIT 2",
    "- SELECT * FROM gap_.t WHERE k = 1 LIMIT 2",
    "- SELECT * FROM gap_.lng",
    "- SELECT * FROM gap_.st",
    "- SELECT * FROM gap_.st WHERE k = 1",
    "- SELECT v FROM gap_.st WHERE k = 1",
    "- SELECT s FROM gap_.st WHERE k = 1",
    "- SELECT * FROM gap_.f WHERE k = 1",
    "- SELECT * FROM gap_.f WHERE k = 1 LIMIT 3",
    "- SELECT * FROM gap_.f WHERE k = 1 AND v > 4 LIMIT 3 ALLOW FILTERING",
    "- SELECT * FROM gap_.f WHERE k = 1 AND v = 7 ALLOW FILTERING",
    "- SELECT * FROM gap_.f WHERE v = 7 ALLOW FILTERING",
    "- SELECT * FROM gap_.f",
    "- SELECT * FROM gap_.f WHERE k = 1 AND c >= 0 AND c < 2000",
    "- SELECT count(*) FROM gap_.f WHERE k = 1",
    "5 SELECT count(*) FROM gap_.f WHERE k = 1",
    "5 SELECT * FROM gap_.f WHERE k = 1 AND v > 4 ALLOW FILTERING",
    "- SELECT count(*) FROM gap_.big WHERE k = 1",
    "- SELECT * FROM gap_.big WHERE k = 1 GROUP BY k, c",
    "2 SELECT * FROM gap_.t WHERE k = 1 GROUP BY k, c",
    "2 SELECT k, c, count(*) FROM gap_.t GROUP BY k",
    "- SELECT k, count(*) FROM gap_.t GROUP BY k",
    "- SELECT * FROM gap_.t WHERE k = 1 GROUP BY k, c LIMIT 3",
    "- SELECT * FROM gap_.t WHERE k = 1 GROUP BY k, c PER PARTITION LIMIT 3",
    "- SELECT * FROM gap_.col WHERE k = 1 AND s CONTAINS 2 ALLOW FILTERING",
    "- SELECT * FROM gap_.col WHERE k = 1 AND m CONTAINS KEY 'a' ALLOW FILTERING",
    "- SELECT * FROM gap_.col WHERE k = 1 AND m['a'] = 1 ALLOW FILTERING",
    "- SELECT * FROM gap_.col WHERE k = 1 AND v IN (3, 4) ALLOW FILTERING",
    "- SELECT * FROM gap_.col WHERE k = 1 AND v = 3 AND m CONTAINS 1 ALLOW FILTERING",
    "- SELECT * FROM gap_.t WHERE k = 1 AND token(k) > 0",
    "- SELECT * FROM gap_.t WHERE k = 1 AND token(k) < 0",
    "- SELECT * FROM gap_.t WHERE token(k) = -4069959284402364209",
    "- SELECT * FROM gap_.u WHERE \"Key\" = 'it''s' ALLOW FILTERING",
    "- SELECT * FROM gap_.u WHERE \"Key\" = 'it''s' AND k2 = 7 AND c2 = 'a' ALLOW FILTERING",
    "- SELECT * FROM gap_.u WHERE c1 > 4000 ALLOW FILTERING",
    "- SELECT * FROM gap_.t WHERE k = 1 AND s = 1 AND v = 2 ALLOW FILTERING",
    "- SELECT count(*), max(v) FROM gap_.t WHERE k = 1",
    "- SELECT * FROM gap_.t WHERE k IN (1, 2)",
    "100 SELECT * FROM gap_.t WHERE k IN (2, 1, 3) LIMIT 5000",
    "- SELECT * FROM gap_.t WHERE k = 1 AND c IN (1, 2, 3, 2000) ORDER BY c DESC",
    "2 SELECT DISTINCT k FROM gap_.t",
    "- SELECT DISTINCT k, s FROM gap_.t",
    "- SELECT * FROM gap_.t WHERE k = 3 AND c >= 2000",
    "- SELECT * FROM gap_.t WHERE k = 1 AND c < 1000",
    "2 SELECT * FROM gap_.t WHERE token(k) > -5000000000000000000 AND token(k) < 0",
    "3 SELECT count(*) FROM gap_.t",
    "- SELECT * FROM gap_.p WHERE k = 5000",
    "- SELECT * FROM gap_.p WHERE v = 1 ALLOW FILTERING",
    "- SELECT c, v FROM gap_.st WHERE k = 1",
    "2 SELECT * FROM gap_.t WHERE k IN (1, 3) PER PARTITION LIMIT 1"
]


def probe():
    sys.path[:0] = [z for z in glob.glob('/opt/cassandra/lib/*.zip') if 'driver' not in z] + \
        ['/opt/cassandra/lib/cassandra-driver-internal-only-3.29.0.zip/cassandra-driver-3.29.0']
    from cassandra.cluster import Cluster
    from cassandra.query import SimpleStatement
    s = Cluster(['127.0.0.1'], protocol_version=4).connect()
    for line in QUERIES:
        fetch, cql = line.split(' ', 1)
        fetch = None if fetch == '-' else int(fetch)
        pages = []
        f = s.execute_async(SimpleStatement(cql, fetch_size=fetch))
        rs = f.result()
        while True:
            pages.append({'rows': len(rs.current_rows), 'warnings': list(f.warnings or [])})
            if not rs.has_more_pages:
                break
            f = rs.response_future
            rs.fetch_next_page()
        print(json.dumps({'fetch': fetch, 'cql': cql, 'pages': pages}))


{'setup': setup, 'probe': probe}[sys.argv[1]]()
