-- Round 16, agent 18: GROUP BY keys as target-list entries, GROUPING() and sub-selects in
-- grouping, SubPlan numbering in subquery_planner's order.
CREATE TABLE a18_i8 (q1 int8, q2 int8);
INSERT INTO a18_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
ANALYZE a18_i8;
CREATE TABLE a18_g5 (id integer PRIMARY KEY, v integer);
INSERT INTO a18_g5 SELECT i, i FROM generate_series(1, 5) i;
ANALYZE a18_g5;
-- The GROUP BY keys the select list doesn't name are resjunk entries of the level's target
-- list: the nodes above the grouping return them too; the grouping's input computes the
-- keys in the target list's order.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT q2 + 1, count(*) FROM a18_i8 GROUP BY q1, q2 + 1 ORDER BY 1;
SELECT q2 + 1, count(*) FROM a18_i8 GROUP BY q1, q2 + 1 ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY q1 ORDER BY sum(q2);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY q1 LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT DISTINCT count(*) FROM a18_i8 GROUP BY q1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*), row_number() OVER () FROM a18_i8 GROUP BY q1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT count(*) c FROM a18_i8 GROUP BY q1 OFFSET 0) s WHERE c > 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (SELECT count(*) FROM a18_i8 b WHERE b.q2 = a.q1 GROUP BY b.q1 ORDER BY 1 LIMIT 1) FROM a18_i8 a;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY q2, q1 ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT q1 FROM a18_i8 GROUP BY q1, q2 ORDER BY q1;
-- A ProjectSet computing the target list over a grouping: the grouping returns what its
-- expressions read, in that order (no grouping sets' mask), then the resjunk keys.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, a) AS g, a + b AS ab FROM (VALUES (1, 1), (2, 2)) AS t (a, b)
GROUP BY ROLLUP(a, ab) ORDER BY 1, 2;
SELECT generate_series(1, a) AS g, a + b AS ab FROM (VALUES (1, 1), (2, 2)) AS t (a, b)
GROUP BY ROLLUP(a, ab) ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, a) AS g, a + b AS ab FROM (VALUES (1, 1), (2, 2)) AS t (a, b)
GROUP BY a, ab ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, 3), count(*) FROM a18_i8 GROUP BY GROUPING SETS (q1, q2) ORDER BY 1;
-- GROUPING() without grouping sets stays GROUPING() (always 0): an aggregation of its own,
-- kept in HAVING, a correlated parameter of a sub-select.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT grouping(q1), q1 FROM a18_i8 GROUP BY q1;
SELECT grouping(q1), q1 FROM a18_i8 GROUP BY q1 ORDER BY 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT grouping(q1) + 1 FROM a18_i8 GROUP BY q1 HAVING grouping(q1) = 0;
SELECT grouping(q1) + 1 FROM a18_i8 GROUP BY q1 HAVING grouping(q1) = 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (SELECT grouping(q1)) FROM a18_i8 GROUP BY q1;
SELECT (SELECT grouping(q1)) FROM a18_i8 GROUP BY q1;
-- Pseudoconstant keys are redundant group pathkeys: out of the Group Key.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY 1 + 1;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY (SELECT 1);
EXPLAIN (COSTS OFF)
SELECT 1 FROM a18_i8 GROUP BY (SELECT 1);
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY q1, (SELECT 1);
SELECT count(*) FROM a18_i8 GROUP BY q1, (SELECT 1) ORDER BY 1;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a18_i8 GROUP BY q1, 1 + 1 ORDER BY 1;
-- A GROUPING() argument that is a sub-select key: a copy of its own, numbered with the
-- target list, before the key's (an RTE_GROUP expression, preprocessed last).
EXPLAIN (COSTS OFF)
SELECT (SELECT grouping(v1)) FROM (VALUES ((SELECT 1))) v(v1) GROUP BY CUBE(v1);
SELECT (SELECT grouping(v1)) FROM (VALUES ((SELECT 1))) v(v1) GROUP BY CUBE(v1);
EXPLAIN (COSTS OFF)
SELECT (SELECT grouping(v1)) FROM (VALUES ((SELECT 1))) v(v1) GROUP BY v1;
SELECT (SELECT grouping(v1)) FROM (VALUES ((SELECT 1))) v(v1) GROUP BY v1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT grouping((SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id)),
       (SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id) AS s
FROM a18_g5 t1
GROUP BY GROUPING SETS(v, s)
ORDER BY CASE WHEN grouping((SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id)) = 0
              THEN (SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id)
              ELSE NULL END NULLS FIRST;
SELECT grouping((SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id)),
       (SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id) AS s
FROM a18_g5 t1
GROUP BY GROUPING SETS(v, s)
ORDER BY CASE WHEN grouping((SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id)) = 0
              THEN (SELECT t1.v FROM a18_g5 t2 WHERE id = t1.id)
              ELSE NULL END NULLS FIRST;
-- A FROM-less LATERAL subquery whose output is a sub-select is pulled up: the sub-select
-- computed where it is read.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT grouping(ss.x) FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss GROUP BY ss.x;
SELECT grouping(ss.x) FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss GROUP BY ss.x;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (SELECT grouping(ss.x)) FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss GROUP BY ss.x;
SELECT (SELECT grouping(ss.x)) FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss GROUP BY ss.x;
-- (Sorted: the Sort refers to the value the scan computes.)
SET enable_hashagg = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (SELECT grouping(ss.x)) FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss GROUP BY ss.x;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT q2 + 1, count(*) FROM a18_i8 GROUP BY q1, q2 + 1 ORDER BY 1;
RESET enable_hashagg;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ss.x FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss;
SELECT ss.x FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ss.x FROM a18_i8 i1 CROSS JOIN (SELECT (SELECT 1) AS x) ss;
SELECT ss.x, ss.x FROM a18_i8 i1 CROSS JOIN LATERAL (SELECT (SELECT i1.q1) AS x) ss ORDER BY 1;
SELECT * FROM a18_i8 i1 LEFT JOIN LATERAL (SELECT (SELECT i1.q1 WHERE i1.q1 > 200) AS x) ss
  ON ss.x IS NOT NULL ORDER BY 1, 2;
-- The join tree's quals' sub-selects are numbered in their order; a scan reading tableoid
-- returns its reltarget (no physical target list).
CREATE TABLE a18_hj1 (a text, b int, id int, c bool);
CREATE TABLE a18_hj2 (a bool, id int, b text, c int);
INSERT INTO a18_hj1 VALUES ('text', 2, 1, false), ('text', 1, 2, false), ('text', 20, 1, false),
  ('text', 1, 1, false);
INSERT INTO a18_hj2 VALUES (true, 1, 'another', 2), (true, 3, 'another', 7), (true, 1, 'another', 90),
  (true, 1, 'another', 3), (true, 1, 'text', 1);
BEGIN;
SET LOCAL enable_sort = off;
SET LOCAL from_collapse_limit = 1;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT a18_hj1.a a1, a18_hj2.a a2, a18_hj1.tableoid::regclass t1, a18_hj2.tableoid::regclass t2
FROM a18_hj1, a18_hj2
WHERE a18_hj1.id = (SELECT 1 WHERE a18_hj2.id = 1)
  AND (SELECT a18_hj1.b * 5) = (SELECT a18_hj2.c * 5)
  AND (SELECT a18_hj1.b * 5) < 50
  AND (SELECT a18_hj2.c * 5) < 55
  AND a18_hj1.a <> a18_hj2.b;
SELECT a18_hj1.a a1, a18_hj2.a a2, a18_hj1.tableoid::regclass t1, a18_hj2.tableoid::regclass t2
FROM a18_hj1, a18_hj2
WHERE a18_hj1.id = (SELECT 1 WHERE a18_hj2.id = 1)
  AND (SELECT a18_hj1.b * 5) = (SELECT a18_hj2.c * 5)
  AND (SELECT a18_hj1.b * 5) < 50
  AND (SELECT a18_hj2.c * 5) < 55
  AND a18_hj1.a <> a18_hj2.b;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT a18_hj1.a a1, a18_hj2.a a2, a18_hj1.tableoid::regclass t1, a18_hj2.tableoid::regclass t2
FROM a18_hj2, a18_hj1
WHERE a18_hj1.id = (SELECT 1 WHERE a18_hj2.id = 1)
  AND (SELECT a18_hj1.b * 5) = (SELECT a18_hj2.c * 5)
  AND (SELECT a18_hj1.b * 5) < 50
  AND (SELECT a18_hj2.c * 5) < 55
  AND a18_hj1.a <> a18_hj2.b;
ROLLBACK;
DROP TABLE a18_i8, a18_g5, a18_hj1, a18_hj2;
