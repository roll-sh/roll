-- A sub-select's WHERE equalities as written: convert_EXISTS_sublink_to_join makes them the
-- semi join's quals, process_equivalence meeting them in that order (the class's member a
-- join clause takes, the order of the join's clauses); an anti join's quals as written, no
-- class taking the outer value in, the right side's own equality its join clause; and the
-- width of an outer side computing a nested loop's PlaceHolderVar parameter (the path's).
CREATE TABLE a55_t1 (a int, b int);
CREATE TABLE a55_t2 (x int, y int);
CREATE TABLE a55_t3 (p int, q int);
INSERT INTO a55_t1 SELECT g, g % 5 FROM generate_series(1, 1000) g;
INSERT INTO a55_t1 VALUES (NULL, 1), (2, NULL);
INSERT INTO a55_t2 SELECT g, g % 3 FROM generate_series(1, 100, 2) g;
INSERT INTO a55_t2 VALUES (NULL, 1), (4, NULL);
INSERT INTO a55_t3 SELECT g, g % 4 FROM generate_series(1, 12, 3) g;
INSERT INTO a55_t3 VALUES (NULL, NULL), (4, 2);
ANALYZE a55_t1, a55_t2, a55_t3;

-- Semi joins: the class's first member on the right side in written order.
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t2.x = t1.a AND t3.p = t2.x);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2 JOIN a55_t3 t3 ON t3.p = t2.x WHERE t2.x = t1.a);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t1.a = t2.x);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t1.a = t3.p AND t3.p = t2.x);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a AND t2.y = t1.b);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t2.y = t1.b AND t3.p = t2.x AND t1.a = t2.x);
SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a) ORDER BY 1, 2;
SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a AND t2.y = t1.b) ORDER BY 1, 2;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE EXISTS (SELECT 1 FROM a55_t2 t2 JOIN a55_t3 t3 ON t3.p = t2.x WHERE t2.x = t1.a);
RESET enable_hashjoin;
RESET enable_mergejoin;

-- Anti joins: the written clauses, the right side joined on its own.
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t2.x = t1.a AND t3.p = t1.a);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2 JOIN a55_t3 t3 ON t3.p = t2.x WHERE t2.x = t1.a AND t3.q = t1.b);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a AND t2.y = t1.b);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a + 1);
EXPLAIN (COSTS OFF) SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = 4 AND t1.a = t3.p);
SELECT * FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a) AND t1.a < 30 OR t1.a IS NULL ORDER BY 1, 2;
SELECT count(*) FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t2.x = t1.a AND t3.p = t1.a);
SELECT count(*) FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2 JOIN a55_t3 t3 ON t3.p = t2.x WHERE t2.x = t1.a AND t3.q = t1.b);
SELECT count(*) FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = t1.a + 1);
SELECT count(*) FROM a55_t1 t1 WHERE NOT EXISTS (SELECT 1 FROM a55_t2 t2, a55_t3 t3 WHERE t3.p = t2.x AND t2.x = 4 AND t1.a = t3.p);
SELECT * FROM a55_t1 t1 LEFT JOIN a55_t3 t3 ON NOT EXISTS (SELECT 1 FROM a55_t2 t2 WHERE t2.x = t3.p AND t2.x = t1.a)
WHERE t1.a < 8 ORDER BY 1, 2, 3, 4;

-- The outer side computing a PlaceHolderVar parameter keeps its path's width.
CREATE TABLE a55_i8 (q1 int8, q2 int8);
INSERT INTO a55_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
CREATE TABLE a55_i4 (f1 int4);
INSERT INTO a55_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
VACUUM ANALYZE a55_i8, a55_i4;
EXPLAIN (VERBOSE)
SELECT *
FROM a55_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a55_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4
WHERE i8.q2 = 123;
SELECT *
FROM a55_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a55_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4
WHERE i8.q2 = 123;

DROP TABLE a55_t1, a55_t2, a55_t3, a55_i8, a55_i4;
