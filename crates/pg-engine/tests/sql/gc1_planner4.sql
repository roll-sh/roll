-- Planner and EXPLAIN shapes (group C1), part 4: TID joins, joins parameterized by
-- several relations, index condition order, NULLS ordering of index scans, LATERAL
-- values inside FULL / RIGHT joins, DML VERBOSE outputs, distinct estimates.
CREATE TABLE gc4_t (id int, v text);
INSERT INTO gc4_t SELECT i, 'v' || i FROM generate_series(1, 200) i;
ANALYZE gc4_t;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc4_t t1 JOIN gc4_t t2 ON t1.ctid = t2.ctid;
SELECT count(*) FROM gc4_t t1 JOIN gc4_t t2 ON t1.ctid = t2.ctid;
SET enable_hashjoin = off;
EXPLAIN (COSTS OFF) SELECT t1.id, t2.id FROM gc4_t t1 JOIN gc4_t t2 ON t1.ctid = t2.ctid WHERE t1.id = 7;
SELECT t1.id, t2.id FROM gc4_t t1 JOIN gc4_t t2 ON t1.ctid = t2.ctid WHERE t1.id = 7;
RESET enable_hashjoin;
-- An index scan parameterized by two other relations.
CREATE TABLE gc4_big (a int, b int, c int);
INSERT INTO gc4_big SELECT i % 1000, i % 37, i FROM generate_series(1, 20000) i;
CREATE INDEX gc4_big_ab ON gc4_big (a, b);
CREATE TABLE gc4_p (q1 int, q2 int);
INSERT INTO gc4_p VALUES (1, 1), (5, 2), (7, 3), (9, 4);
ANALYZE gc4_big, gc4_p;
EXPLAIN (COSTS OFF) SELECT * FROM gc4_big, gc4_p x, gc4_p y WHERE gc4_big.a = x.q1 AND gc4_big.b = y.q1 AND x.q2 = 1 AND y.q2 = 2;
SELECT * FROM gc4_big, gc4_p x, gc4_p y WHERE gc4_big.a = x.q1 AND gc4_big.b = y.q1 AND x.q2 = 1 AND y.q2 = 2 ORDER BY c;
-- Index conditions in the order written.
EXPLAIN (COSTS OFF) SELECT c FROM gc4_big WHERE a = ANY ('{1,2,3}') AND a < 3::bigint;
EXPLAIN (COSTS OFF) SELECT c FROM gc4_big WHERE a IN (1, 42, 7) AND a = 1;
-- NULLS LAST / FIRST of a descending index.
CREATE TABLE gc4_n (f1 int);
INSERT INTO gc4_n VALUES (42), (3), (NULL), (10), (NULL), (1);
CREATE INDEX gc4_n_i ON gc4_n (f1 DESC NULLS LAST);
SET enable_sort = off;
EXPLAIN (COSTS OFF) SELECT * FROM gc4_n ORDER BY f1 NULLS FIRST;
SELECT * FROM gc4_n ORDER BY f1 NULLS FIRST;
SELECT * FROM gc4_n ORDER BY f1 DESC NULLS LAST;
SELECT * FROM gc4_n WHERE f1 > 2 ORDER BY f1 DESC NULLS LAST;
RESET enable_sort;
-- An enclosing query's value inside a FULL / RIGHT join.
SELECT * FROM (VALUES (1), (2)) v(r) LEFT JOIN LATERAL (SELECT * FROM generate_series(1, v.r) a FULL JOIN generate_series(1, v.r) b ON false) s ON true ORDER BY 1, 2, 3;
SELECT * FROM (VALUES (1), (2)) v(r), LATERAL (SELECT * FROM generate_series(1, 2) a RIGHT JOIN (SELECT v.r AS z) b ON a = b.z) s ORDER BY 1, 2;
-- DML under VERBOSE.
CREATE TABLE gc4_d (f1 int, f2 text, f3 int DEFAULT 42, f4 bigint DEFAULT 99);
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO gc4_d VALUES (4) RETURNING *;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE gc4_d SET f3 = f3 + 1, f2 = 'x' WHERE f1 = 3 RETURNING f1, f3;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM gc4_d WHERE f1 = 3 RETURNING f2;
-- Distinct counts of small set-returning functions' columns.
EXPLAIN (COSTS OFF) SELECT DISTINCT (i || '/' || j)::pg_lsn f FROM generate_series(1, 10) i, generate_series(1, 10) j WHERE i <= 10 AND j > 0 AND j <= 10 ORDER BY f;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM (SELECT DISTINCT a, b, a FROM gc4_big) ss;
-- A sub-select in a window function's argument: its InitPlan.
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT a, count((SELECT 1)) OVER (ORDER BY a DESC) c FROM gc4_p, (SELECT q1 AS a FROM gc4_p) z) s WHERE c = 1;
