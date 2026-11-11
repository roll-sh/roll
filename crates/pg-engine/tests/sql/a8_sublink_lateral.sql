-- IN over a sub-select with a set-returning select list: a semi join with the sub-select
-- as a subquery ("ANY_subquery"); its ProjectSet returns the calls' values first. A
-- clause reading a LATERAL value stays a Subquery Scan's filter (qual_is_pushdown_safe).
-- IN over a VALUES list with a correlated sub-select inside: a LATERAL semi join.
CREATE TABLE a8_sl (f1 int);
INSERT INTO a8_sl VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
ANALYZE a8_sl;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_sl o WHERE (f1, f1) IN
  (SELECT f1, generate_series(1, 50) / 10 g FROM a8_sl i GROUP BY f1);
SELECT * FROM a8_sl o WHERE (f1, f1) IN
  (SELECT f1, generate_series(1, 50) / 10 g FROM a8_sl i GROUP BY f1);
EXPLAIN (VERBOSE, COSTS OFF) SELECT f1, generate_series(1, 3) / 10 g FROM a8_sl;
EXPLAIN (VERBOSE, COSTS OFF) SELECT f1 + generate_series(1, 2), f1 FROM a8_sl;
SELECT f1, generate_series(1, 3) / 10 g FROM a8_sl ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_sl i4,
  LATERAL (
    SELECT i4.f1 > 1 AS b, 1 AS id FROM (SELECT random() ORDER BY 1) AS t1
    UNION ALL
    SELECT true AS b, 2 AS id
  ) AS t2
WHERE b AND f1 >= 0;
SELECT * FROM a8_sl i4,
  LATERAL (
    SELECT i4.f1 > 1 AS b, 1 AS id FROM (SELECT random() ORDER BY 1) AS t1
    UNION ALL
    SELECT true AS b, 2 AS id
  ) AS t2
WHERE b AND f1 >= 0 ORDER BY 1, 3;
CREATE TABLE a8_sv (unique1 int, unique2 int, ten int);
INSERT INTO a8_sv SELECT i, 9 - i, i % 3 FROM generate_series(0, 9) i;
CREATE INDEX a8_sv_u2 ON a8_sv (unique2);
ANALYZE a8_sv;
EXPLAIN (COSTS OFF)
SELECT ten FROM a8_sv t
WHERE unique1 IN (VALUES (0), ((2 IN (SELECT unique2 FROM a8_sv c WHERE c.unique2 = t.unique1))::integer));
SELECT ten FROM a8_sv t
WHERE unique1 IN (VALUES (0), ((2 IN (SELECT unique2 FROM a8_sv c WHERE c.unique2 = t.unique1))::integer))
ORDER BY 1;
DROP TABLE a8_sv;
DROP TABLE a8_sl;
