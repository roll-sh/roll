-- Plan shapes: LATERAL references across join levels, FROM-less subqueries and one-row
-- VALUES pulled up (empty relations removed), grouping expressions computed below the
-- aggregation, JSON aggregates under a node computing their constructor, user-defined
-- operators in IN (...) sublinks, a LIMIT's bound through a subquery's scan.
CREATE TABLE gc2_a (id int PRIMARY KEY, v int);
CREATE TABLE gc2_b (id int PRIMARY KEY, v int);
INSERT INTO gc2_a SELECT g, g % 7 FROM generate_series(1, 200) g;
INSERT INTO gc2_b SELECT g, g % 5 FROM generate_series(1, 200) g;
ANALYZE gc2_a;
ANALYZE gc2_b;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gc2_a a, gc2_b b JOIN LATERAL (VALUES (a.id)) ss(x) ON b.id = ss.x;
SELECT count(*) FROM gc2_a a, gc2_b b JOIN LATERAL (VALUES (a.id)) ss(x) ON b.id = ss.x;
SELECT count(*) FROM gc2_a a, gc2_b b JOIN LATERAL (VALUES (a.id), (-1)) ss(x) ON b.id = ss.x;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 1 AS x) ss1 LEFT JOIN (SELECT 2 AS y) ss2 ON (true),
  LATERAL (SELECT ss2.y AS z LIMIT 1) ss3;
SELECT * FROM (SELECT 1 AS x) ss1 LEFT JOIN (SELECT 2 AS y) ss2 ON (true),
  LATERAL (SELECT ss2.y AS z LIMIT 1) ss3;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT a.id AS x FROM gc2_a a WHERE false) q1,
  (SELECT b.id AS y FROM gc2_b b WHERE false) q2 WHERE q1.x = q2.y;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT v % 3, count(*) FROM gc2_a GROUP BY v % 3;
SET enable_hashagg = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT v % 3, count(*) FROM gc2_a GROUP BY v % 3;
RESET enable_hashagg;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT i % 2 AS g, JSON_ARRAYAGG(i ORDER BY i RETURNING jsonb) AS ja,
  JSON_ARRAYAGG(i ORDER BY i RETURNING text) AS ja_text,
  row_number() OVER (ORDER BY i % 2) AS rn
FROM generate_series(1, 3) i GROUP BY i % 2;
SELECT i % 2 AS g, JSON_ARRAYAGG(i ORDER BY i RETURNING jsonb) AS ja,
  JSON_ARRAYAGG(i ORDER BY i RETURNING text) AS ja_text,
  row_number() OVER (ORDER BY i % 2) AS rn
FROM generate_series(1, 3) i GROUP BY i % 2;

CREATE TABLE gc2_txt (c1 text, c2 text);
INSERT INTO gc2_txt VALUES ('a', NULL), ('123', '456');
CREATE TABLE gc2_i8 (q1 int8, q2 int8);
INSERT INTO gc2_i8 VALUES (123, 456), (4567890123456789, 123);
CREATE FUNCTION gc2_int8_text_eq(int8, text) RETURNS boolean LANGUAGE sql AS 'select $1::text = $2';
CREATE OPERATOR = (PROCEDURE = gc2_int8_text_eq, LEFTARG = int8, RIGHTARG = text);
EXPLAIN (COSTS OFF) SELECT * FROM gc2_i8 WHERE q1 IN (SELECT c1 FROM gc2_txt);
SELECT * FROM gc2_i8 WHERE q1 IN (SELECT c1 FROM gc2_txt);
CREATE OR REPLACE FUNCTION gc2_int8_text_eq(int8, text) RETURNS boolean LANGUAGE sql AS 'select $2 = $1::text';
EXPLAIN (COSTS OFF) SELECT * FROM gc2_i8 WHERE q1 IN (SELECT c1 FROM gc2_txt);
SELECT * FROM gc2_i8 WHERE q1 IN (SELECT c1 FROM gc2_txt);
DROP OPERATOR = (int8, text);
DROP FUNCTION gc2_int8_text_eq(int8, text);

EXPLAIN (ANALYZE, SUMMARY OFF, TIMING OFF, COSTS OFF, BUFFERS OFF)
SELECT * FROM (SELECT id, v FROM gc2_a ORDER BY v, id) AS x LIMIT 3;

-- A SubPlan in a join's equality is a hash key, computed by the Hash; one in an IN's
-- left operand stays in the semi join's clause; one in a VALUES list is its scan's.
EXPLAIN (COSTS OFF)
SELECT a.id, b.id FROM gc2_a a JOIN gc2_b b ON a.v = b.v
  AND a.id = (SELECT min(id) FROM gc2_b c WHERE c.id = b.id)
WHERE a.id < 10 AND b.id < 10 ORDER BY a.id, b.id;
SELECT a.id, b.id FROM gc2_a a JOIN gc2_b b ON a.v = b.v
  AND a.id = (SELECT min(id) FROM gc2_b c WHERE c.id = b.id)
WHERE a.id < 10 AND b.id < 10 ORDER BY a.id, b.id;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM gc2_i8 WHERE (CASE WHEN q2 IN (SELECT id FROM gc2_a) THEN q2 END) IN (SELECT v FROM gc2_b);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (VALUES (3 NOT IN (SELECT * FROM (VALUES (1), (2)) s1)), (false)) s;
SELECT * FROM (VALUES (3 NOT IN (SELECT * FROM (VALUES (1), (2)) s1)), (false)) s;

-- A subquery planned apart keeps its InitPlans and its volatile outputs; a SubPlan
-- reading only values of the level above it is hashed.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, x FROM (SELECT (SELECT random()) AS x FROM (VALUES (1), (2)) v(y)) ss;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT sum(ss.t::int) FROM gc2_a o CROSS JOIN LATERAL (
  SELECT i.v IN (SELECT id FROM gc2_b WHERE id <= o.v) AS t, random() AS r
  FROM gc2_a i WHERE i.id = o.id) ss
WHERE o.v = 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (SELECT 1) AS x, g FROM generate_series(1, 3) g ORDER BY g LIMIT 2;

-- A set-returning GROUP BY expression: its ProjectSet returns just what the
-- aggregation reads.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, v) AS g, count(*) FROM gc2_a GROUP BY 1;

-- An anonymous row's IS NULL tests each field, nested rows not recursed into.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT r, r IS NULL AS isnull, r IS NOT NULL AS isnotnull
FROM (VALUES (1, row(1, 2)), (1, row(NULL, NULL)), (1, NULL), (NULL, row(NULL, NULL)), (NULL, NULL)) r(a, b);
SELECT r, r IS NULL AS isnull, r IS NOT NULL AS isnotnull
FROM (VALUES (1, row(1, 2)), (1, row(NULL, NULL)), (1, NULL), (NULL, row(NULL, NULL)), (NULL, NULL)) r(a, b);

-- CTEs not inlined: one locking rows, one read twice reading the recursive query's
-- working table (its CTE the recursive term's).
WITH x AS (SELECT * FROM (SELECT id FROM gc2_a FOR UPDATE) ss) SELECT * FROM x WHERE id = 1;
EXPLAIN (VERBOSE, COSTS OFF)
WITH RECURSIVE x(a) AS (
  (VALUES ('a'), ('b'))
  UNION ALL
  (WITH z AS NOT MATERIALIZED (SELECT * FROM x)
   SELECT z.a || z1.a AS a FROM z CROSS JOIN z AS z1 WHERE length(z.a || z1.a) < 5))
SELECT * FROM x;
WITH RECURSIVE x(a) AS (
  (VALUES ('a'), ('b'))
  UNION ALL
  (WITH z AS NOT MATERIALIZED (SELECT * FROM x)
   SELECT z.a || z1.a AS a FROM z CROSS JOIN z AS z1 WHERE length(z.a || z1.a) < 5))
SELECT * FROM x ORDER BY a;
