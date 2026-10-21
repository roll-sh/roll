-- Pulled-up FROM subqueries' computed outputs (pullup_replace_vars_callback): evaluated
-- where the query reads them unless a PlaceHolderVar is needed (an outer join nulls them
-- and they wouldn't go NULL with their table's row), LATERAL references that need no
-- PlaceHolderVar no longer forcing a nested loop, a LATERAL subquery's WHERE clauses as
-- join clauses, and a PlaceHolderVar after its relation's Vars in the scan's output.
CREATE TABLE gc1_i8 (q1 int8, q2 int8);
INSERT INTO gc1_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
VACUUM gc1_i8;
CREATE TABLE gc1_t (unique1 int, ten int, fivethous int);
INSERT INTO gc1_t SELECT g, g % 10, g % 5000 FROM generate_series(0, 9999) g;
ANALYZE gc1_t;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN (SELECT q1 + 1 AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2;
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN (SELECT q1 + 1 AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2
ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN (SELECT coalesce(q1, 0) AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN (SELECT 1 AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN LATERAL (SELECT t1.q1 + 1 AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN LATERAL (SELECT t1.q1 + t2.q1 AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2;
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN LATERAL (SELECT t1.q1 + t2.q1 AS x, q2 FROM gc1_i8 t2) ss ON t1.q2 = ss.q2
ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM gc1_i8 t1 LEFT JOIN LATERAL (SELECT * FROM gc1_i8 t2 WHERE t2.q1 = t1.q2) ss ON true;
SELECT * FROM gc1_i8 t1 LEFT JOIN LATERAL (SELECT * FROM gc1_i8 t2 WHERE t2.q1 = t1.q2) ss ON true
ORDER BY 1, 2, 3, 4;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.ten, sum(x) FROM gc1_t t1 LEFT JOIN LATERAL (
  SELECT t1.ten + t2.ten AS x, t2.fivethous FROM gc1_t t2) ss ON t1.unique1 = ss.fivethous
GROUP BY t1.ten ORDER BY t1.ten;
SELECT t1.ten, sum(x) FROM gc1_t t1 LEFT JOIN LATERAL (
  SELECT t1.ten + t2.ten AS x, t2.fivethous FROM gc1_t t2) ss ON t1.unique1 = ss.fivethous
GROUP BY t1.ten ORDER BY t1.ten;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN
  (gc1_i8 t2 LEFT JOIN LATERAL (SELECT t2.q1 + 1 AS x, * FROM gc1_i8 t3) t3 ON t2.q2 = t3.q2)
  ON t1.q2 = t2.q2
ORDER BY 1, 2;
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN
  (gc1_i8 t2 LEFT JOIN LATERAL (SELECT t2.q1 + 1 AS x, * FROM gc1_i8 t3) t3 ON t2.q2 = t3.q2)
  ON t1.q2 = t2.q2
ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.q1, x FROM gc1_i8 t1 LEFT JOIN
  (gc1_i8 t2 INNER JOIN LATERAL (SELECT t2.q1 + 1 AS x, * FROM gc1_i8 t3) t3 ON t2.q2 = t3.q2)
  ON t1.q2 = t2.q2
ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ss2.* FROM gc1_i8 t1 LEFT JOIN
  (gc1_i8 t2 LEFT JOIN (SELECT coalesce(q1) AS x, * FROM gc1_i8 t3) ss1 ON t2.q1 = ss1.q2
   LEFT JOIN LATERAL (SELECT ss1.x AS y, * FROM gc1_i8 t4) ss2 ON t2.q2 = ss2.q1)
  ON t1.q2 = ss2.q1
ORDER BY 1, 2, 3;
SELECT ss2.* FROM gc1_i8 t1 LEFT JOIN
  (gc1_i8 t2 LEFT JOIN (SELECT coalesce(q1) AS x, * FROM gc1_i8 t3) ss1 ON t2.q1 = ss1.q2
   LEFT JOIN LATERAL (SELECT ss1.x AS y, * FROM gc1_i8 t4) ss2 ON t2.q2 = ss2.q1)
  ON t1.q2 = ss2.q1
ORDER BY 1, 2, 3;

-- A domain without constraints over a column is the column itself in a qual; one with
-- constraints shows its relabeling to the base type. A catalog keeps its alias.
CREATE DOMAIN gc1_free AS int;
CREATE DOMAIN gc1_pos AS int CHECK (VALUE >= 0);
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT unique1::gc1_free AS x FROM gc1_t) s WHERE x = 1;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT unique1::gc1_pos AS x FROM gc1_t) s WHERE x = 1;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT relname::information_schema.sql_identifier AS tname, * FROM
  (SELECT * FROM pg_class c) ss1) ss2 WHERE tname = 'gc1_t';
SELECT tname FROM (SELECT relname::information_schema.sql_identifier AS tname, * FROM
  (SELECT * FROM pg_class c) ss1) ss2 WHERE tname = 'gc1_t';
