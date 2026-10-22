-- A pulled-up subquery's sub-select read twice is two SubPlans / InitPlans; an ORDER BY
-- key constant for the query (an InitPlan's value) is dropped; a pseudoconstant WHERE
-- clause gates the whole join (and `now() = now()` stays as written); a join under a
-- gating Result returns the columns needed above; a scan under a Sort / Hash returns its
-- reltarget (what the target list reads of it first), joins each relation's in that order;
-- a correlated EXISTS over an inheritance tree picks its hashed alternative per member.
CREATE TABLE a3_pt (f1 int, f2 int, f3 float8);
INSERT INTO a3_pt VALUES (1, 2, 3), (2, 3, 4), (3, 4, 5), (1, 1, 1);
CREATE TABLE a3_px (x int, y int, z int);
CREATE TABLE a3_py (x int, y int, z int);
CREATE TABLE a3_pz (x int, y int, z int);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, x FROM (SELECT (SELECT now()) AS x FROM (VALUES (1), (2)) v(y)) ss;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, x FROM (SELECT (SELECT now() WHERE y = y) AS x FROM (VALUES (1), (2)) v(y)) ss;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, x + interval '1 day' FROM (SELECT (SELECT now()) AS x FROM (VALUES (1), (2)) v(y)) ss;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x FROM (SELECT (SELECT now()) AS x FROM (VALUES (1), (2)) v(y)) ss ORDER BY x;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, x FROM (SELECT (SELECT max(f2) FROM a3_pt q2 WHERE q2.f1 = q1.f1) AS x FROM a3_pt q1) ss;
SELECT x, x FROM (SELECT (SELECT max(f2) FROM a3_pt q2 WHERE q2.f1 = q1.f1) AS x FROM a3_pt q1) ss ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (SELECT 1), (SELECT 1) FROM a3_pt;
EXPLAIN (VERBOSE, COSTS OFF)
WITH x AS NOT MATERIALIZED (SELECT * FROM (SELECT f1, now() AS n FROM a3_pt) ss)
SELECT * FROM x, x x2 WHERE x.n = x2.n;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a3_pt a, a3_pt b WHERE now() = now();
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a.f1, b.f2 FROM a3_pt a, a3_pt b WHERE a.f1 = b.f1 AND now() = now();
SELECT count(*) FROM a3_pt a, a3_pt b WHERE a.f1 = b.f1 AND now() = now();
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a3_px.z, a3_px.y, a3_py.z FROM a3_px JOIN a3_py ON a3_px.x = a3_py.x JOIN a3_pz ON a3_pz.y = a3_py.y;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a3_px.z, a3_px.y, a3_py.z, a3_px.ctid FROM a3_px JOIN a3_py ON a3_px.x = a3_py.x;
CREATE TABLE a3_pe (c1 int, c2 int, c3 int) PARTITION BY LIST (c1);
CREATE TABLE a3_pe_null PARTITION OF a3_pe FOR VALUES IN (NULL);
CREATE TABLE a3_pe_def PARTITION OF a3_pe DEFAULT;
INSERT INTO a3_pe SELECT x, x / 2, x + 1 FROM generate_series(0, 10) x;
ANALYZE a3_pe;
EXPLAIN (COSTS OFF)
SELECT * FROM a3_pe t1 WHERE (EXISTS (SELECT 1 FROM a3_pe t2 WHERE t1.c1 = t2.c2) OR c3 < 0);
SELECT * FROM a3_pe t1 WHERE (EXISTS (SELECT 1 FROM a3_pe t2 WHERE t1.c1 = t2.c2) OR c3 < 0) ORDER BY 1;
DROP TABLE a3_pt, a3_px, a3_py, a3_pz, a3_pe;
