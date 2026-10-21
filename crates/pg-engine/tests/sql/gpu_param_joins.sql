-- `x IN (SELECT <one row without FROM>)` pulled up as its comparison; join paths parameterized
-- by an outer join's left side (param_source_rels), the join clauses from there their own;
-- one equivalence-class clause per parameterized path.
CREATE TABLE gpu_t (unique1 int, unique2 int, two int, ten int, hundred int, thousand int, tenthous int);
INSERT INTO gpu_t SELECT i, (i * 7919) % 10000, i % 2, i % 10, i % 100, i % 1000, i FROM generate_series(0, 9999) i;
CREATE INDEX gpu_t_unique1 ON gpu_t (unique1);
CREATE INDEX gpu_t_unique2 ON gpu_t (unique2);
CREATE INDEX gpu_t_hundred ON gpu_t (hundred);
CREATE INDEX gpu_t_thous_tenthous ON gpu_t (thousand, tenthous);
ANALYZE gpu_t;
CREATE TABLE gpu_i4 (f1 int);
INSERT INTO gpu_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
ANALYZE gpu_i4;

EXPLAIN (COSTS OFF)
SELECT unique1 FROM gpu_t WHERE unique1 IN (SELECT 17);
SELECT unique1 FROM gpu_t WHERE unique1 IN (SELECT 17);
EXPLAIN (COSTS OFF)
SELECT unique1 FROM gpu_t WHERE (unique1, unique2) IN (SELECT 17, 4);
SELECT unique1, unique2 FROM gpu_t WHERE (unique1, unique2) IN (SELECT 17, 3);
CREATE FUNCTION gpu_imm(i int) RETURNS int AS $$ BEGIN RETURN i; END; $$ LANGUAGE plpgsql IMMUTABLE;
EXPLAIN (COSTS OFF)
SELECT unique1 FROM gpu_t, LATERAL gpu_imm(1) x WHERE x IN (SELECT 17);
-- (In the select list it stays a SubPlan.)
EXPLAIN (COSTS OFF)
SELECT unique1 IN (SELECT 17) FROM gpu_t WHERE unique1 < 3;
SELECT unique1 IN (SELECT 17) FROM gpu_t WHERE unique1 < 3 ORDER BY 1;

-- The nullable side's join parameterized by the left side, its clauses checked inside.
EXPLAIN (COSTS OFF)
SELECT * FROM gpu_t t1 LEFT JOIN
  (gpu_t t2 JOIN gpu_t t3 ON t2.thousand = t3.unique2)
  ON t1.hundred = t2.hundred AND t1.ten = t3.ten
WHERE t1.unique1 = 1;
SELECT count(*), sum(t3.unique1) FROM gpu_t t1 LEFT JOIN
  (gpu_t t2 JOIN gpu_t t3 ON t2.thousand = t3.unique2)
  ON t1.hundred = t2.hundred AND t1.ten = t3.ten
WHERE t1.unique1 = 1;
EXPLAIN (COSTS OFF)
SELECT * FROM gpu_t t1 LEFT JOIN
  (gpu_t t2 JOIN gpu_t t3 ON t2.thousand = t3.unique2)
  ON t1.hundred = t2.hundred AND t1.ten + t2.ten = t3.ten
WHERE t1.unique1 = 1;
SELECT count(*), sum(t3.unique1) FROM gpu_t t1 LEFT JOIN
  (gpu_t t2 JOIN gpu_t t3 ON t2.thousand = t3.unique2)
  ON t1.hundred = t2.hundred AND t1.ten + t2.ten = t3.ten
WHERE t1.unique1 IN (1, 2, 3);

-- One clause of an equivalence class in a parameterized path; the other members joined
-- below.
EXPLAIN (COSTS OFF)
SELECT * FROM gpu_t t,
  (SELECT sum(f1) + 1 AS f1 FROM gpu_i4 i4a) a,
  (SELECT sum(f1) AS f1 FROM gpu_i4 i4b) b
WHERE b.f1 = t.thousand AND a.f1 = b.f1;
SELECT count(*) FROM gpu_t t,
  (SELECT sum(f1) + 1 AS f1 FROM gpu_i4 i4a) a,
  (SELECT sum(f1) AS f1 FROM gpu_i4 i4b) b
WHERE b.f1 = t.thousand AND a.f1 - 1 = b.f1;
