-- Subqueries over joins pulled up into the parent's join tree, a pulled-up subquery's WHERE
-- kept below the outer join above it, join clauses enforced in a parameterized inner scan,
-- dummy relations keeping their outputs (and a constant-false qual at the outer join above),
-- partitionwise child joins with parameterized index paths.
CREATE TABLE gpu_n1 (id int PRIMARY KEY, a1 boolean, a2 boolean);
CREATE TABLE gpu_n2 (id int PRIMARY KEY, n1_id int, b1 boolean, b2 boolean);
CREATE TABLE gpu_n3 (id int PRIMARY KEY, n2_id int, c1 boolean);
INSERT INTO gpu_n1 VALUES (1, true, true), (2, true, false), (3, false, false);
INSERT INTO gpu_n2 VALUES (1, 1, true, true), (2, 2, true, false), (3, 3, false, false);
INSERT INTO gpu_n3 VALUES (1, 1, true), (2, 2, false), (3, 3, true);

-- Both outer joins reduced: the subquery over a join is part of the group.
SELECT n3.id FROM gpu_n3 n3
  LEFT JOIN (SELECT n2.*, (n2.b1 AND ss1.a3) AS b3
             FROM gpu_n2 n2
               LEFT JOIN (SELECT n1.*, (n1.id IS NOT NULL) AS a3 FROM gpu_n1 n1) ss1
               ON ss1.id = n2.n1_id) ss2
  ON ss2.id = n3.n2_id
WHERE n3.id = 1 AND ss2.b3;
SELECT n3.id, ss2.* FROM gpu_n3 n3
  LEFT JOIN (SELECT n2.id, n1.a1, coalesce(n1.a2, n2.b2) AS c
             FROM gpu_n2 n2 JOIN gpu_n1 n1 ON n1.id = n2.n1_id
             WHERE n1.id > 1) ss2
  ON ss2.id = n3.n2_id
ORDER BY 1;

-- Join clauses between a parameterized inner scan and its outer relations are its filter.
EXPLAIN (COSTS OFF)
SELECT n3.id FROM gpu_n3 n3 JOIN gpu_n2 n2 ON n2.id = n3.n2_id JOIN gpu_n1 n1 ON n1.id = n2.n1_id
WHERE n3.id = 1 AND (n2.b1 OR n1.a1);
EXPLAIN (COSTS OFF)
SELECT n3.id FROM gpu_n3 n3 JOIN gpu_n2 n2 ON n2.id = n3.n2_id JOIN gpu_n1 n1 ON n1.id = n2.n1_id
WHERE n3.id = 1 AND CASE WHEN n2.b1 THEN n1.id IS NOT NULL ELSE false END;
SELECT n3.id FROM gpu_n3 n3 JOIN gpu_n2 n2 ON n2.id = n3.n2_id JOIN gpu_n1 n1 ON n1.id = n2.n1_id
WHERE n3.id = 1 AND (n2.b1 OR n1.a1);

-- A subquery's WHERE over its own outer join stays below the outer join above it.
CREATE TABLE gpu_t3 (f1 int, f2 text);
INSERT INTO gpu_t3 SELECT x, repeat('xyzzy', 100) FROM generate_series(1, 10000) x;
ANALYZE gpu_t3;
CREATE TABLE gpu_t4 (f1 int);
INSERT INTO gpu_t4 VALUES (0), (1), (9999);
ANALYZE gpu_t4;
EXPLAIN (COSTS OFF)
SELECT a.f1 FROM gpu_t4 a
  LEFT JOIN (SELECT b.f1 FROM gpu_t3 b LEFT JOIN gpu_t3 c ON (b.f1 = c.f1)
             WHERE COALESCE(c.f1, 0) = 0) AS d ON (a.f1 = d.f1)
WHERE COALESCE(d.f1, 0) = 0 ORDER BY 1;
SELECT a.f1 FROM gpu_t4 a
  LEFT JOIN (SELECT b.f1 FROM gpu_t3 b LEFT JOIN gpu_t3 c ON (b.f1 = c.f1)
             WHERE COALESCE(c.f1, 0) = 0) AS d ON (a.f1 = d.f1)
WHERE COALESCE(d.f1, 0) = 0 ORDER BY 1;

-- A dummy relation keeps its outputs; a pulled-up FROM-less WHERE false is the outer
-- join's filter.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (VALUES (1), (2)) v(x)
  LEFT JOIN (SELECT q FROM (SELECT 7 AS q FROM (SELECT WHERE false) ss1) ss2
             LEFT JOIN (SELECT 8 AS z) ss3 ON true) ss4 ON true;
SELECT * FROM (VALUES (1), (2)) v(x)
  LEFT JOIN (SELECT q FROM (SELECT 7 AS q FROM (SELECT WHERE false) ss1) ss2
             LEFT JOIN (SELECT 8 AS z) ss3 ON true) ss4 ON true;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (VALUES (1), (2)) v(x) LEFT JOIN (SELECT 7 AS q WHERE false) ss ON true;

-- A lateral PlaceHolderVar computed at a join, read above it.
CREATE TABLE gpu_i8 (q1 int8, q2 int8);
INSERT INTO gpu_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
CREATE TABLE gpu_i4 (f1 int);
INSERT INTO gpu_i4 VALUES (0), (1), (123456);
ANALYZE gpu_i4;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT i8.*, ss.v FROM gpu_i8 i8
  LEFT JOIN gpu_i4 i4 ON i4.f1 = 1
  LEFT JOIN LATERAL (SELECT i4.f1 + 1 AS v) AS ss ON true
  LEFT JOIN gpu_n1 t ON t.id = ss.v
WHERE q2 = 456;

-- Partitionwise child joins plan their partitions again, parameterized paths costed for
-- the parent relations' rows.
SET enable_partitionwise_join = on;
CREATE TABLE gpu_p1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE gpu_p1_p1 PARTITION OF gpu_p1 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_p1_p3 PARTITION OF gpu_p1 FOR VALUES FROM (500) TO (600);
CREATE TABLE gpu_p1_p2 PARTITION OF gpu_p1 FOR VALUES FROM (250) TO (500);
INSERT INTO gpu_p1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE INDEX gpu_p1_p1_a ON gpu_p1_p1 (a);
CREATE INDEX gpu_p1_p2_a ON gpu_p1_p2 (a);
CREATE INDEX gpu_p1_p3_a ON gpu_p1_p3 (a);
ANALYZE gpu_p1;
CREATE TABLE gpu_p2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE gpu_p2_p1 PARTITION OF gpu_p2 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_p2_p2 PARTITION OF gpu_p2 FOR VALUES FROM (250) TO (500);
CREATE TABLE gpu_p2_p3 PARTITION OF gpu_p2 FOR VALUES FROM (500) TO (600);
INSERT INTO gpu_p2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
CREATE INDEX gpu_p2_p1_b ON gpu_p2_p1 (b);
CREATE INDEX gpu_p2_p2_b ON gpu_p2_p2 (b);
CREATE INDEX gpu_p2_p3_b ON gpu_p2_p3 (b);
ANALYZE gpu_p2;
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_p1 t1 RIGHT JOIN gpu_p2 t2 ON t1.a = t2.b WHERE t2.a = 0 ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_p1 t1 RIGHT JOIN gpu_p2 t2 ON t1.a = t2.b WHERE t2.a = 0 ORDER BY t1.a, t2.b;
RESET enable_partitionwise_join;
