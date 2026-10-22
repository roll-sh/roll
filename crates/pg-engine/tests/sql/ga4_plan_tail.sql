-- Round 14 (agent 4): a correlated SubPlan's parameterized inner scan reading the outer
-- query's value (wrong results before), join clauses enforced in a parameterized path,
-- partial-index quals kept for row-locked scans, index-only scans of expression keys,
-- resjunk ORDER BY keys of ordered scans, dummy relations through subqueries, void SQL
-- functions, sort keys over domains and declared collations, constraint exclusion over
-- virtual generated columns, hashed ANY subqueries in qual order.
CREATE TABLE ga4_tk (unique1 int, unique2 int, two int, four int, ten int, twenty int, hundred int, thousand int, tenthous int);
INSERT INTO ga4_tk SELECT i, (i * 7919) % 10000, i % 2, i % 4, i % 10, i % 20, i % 100, i % 1000, i
  FROM generate_series(0, 9999) i;
CREATE INDEX ga4_tk_u1 ON ga4_tk (unique1);
CREATE INDEX ga4_tk_hundred ON ga4_tk (hundred);
VACUUM ANALYZE ga4_tk;

-- The SubPlan's own outer value (t0.ten) inside the Memoize's parameterized scan.
EXPLAIN (COSTS OFF)
SELECT unique1 FROM ga4_tk t0
WHERE unique1 < 3
  AND EXISTS (
	SELECT 1 FROM ga4_tk t1
	INNER JOIN ga4_tk t2 ON t1.unique1 = t2.hundred
	WHERE t0.ten = t1.twenty AND t0.two <> t2.four OFFSET 0);
SELECT unique1 FROM ga4_tk t0
WHERE unique1 < 3
  AND EXISTS (
	SELECT 1 FROM ga4_tk t1
	INNER JOIN ga4_tk t2 ON t1.unique1 = t2.hundred
	WHERE t0.ten = t1.twenty AND t0.two <> t2.four OFFSET 0);

-- Join clauses no index takes: the parameterized scan's filter, the cache keys in
-- ppi_clauses' order.
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (COSTS OFF)
SELECT * FROM ga4_tk t1 JOIN ga4_tk t2 ON t1.unique1 = t2.hundred AND t1.ten = t2.four WHERE t2.unique1 < 50;
EXPLAIN (COSTS OFF)
SELECT * FROM ga4_tk t1 JOIN ga4_tk t2 ON t1.unique1 = t2.hundred AND t1.ten < t2.four WHERE t2.unique1 < 50;
CREATE TABLE ga4_ek (x numeric, t text);
INSERT INTO ga4_ek (x, t) SELECT d1::numeric, d1::text
  FROM (SELECT round((d / pi())::numeric, 7) AS d1 FROM generate_series(1, 20) AS d) t;
INSERT INTO ga4_ek SELECT * FROM ga4_ek;
CREATE INDEX ga4_ek_idx_x_t ON ga4_ek (x, t);
VACUUM ANALYZE ga4_ek;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF)
SELECT * FROM ga4_ek t1 INNER JOIN ga4_ek t2 ON t1.x = t2.t::numeric AND t1.t::numeric = t2.x;
SELECT count(*) FROM ga4_ek t1 INNER JOIN ga4_ek t2 ON t1.x = t2.t::numeric AND t1.t::numeric = t2.x;
RESET enable_seqscan;
RESET enable_hashjoin;
RESET enable_mergejoin;

-- A partial index's implied quals stay for a row-locked scan, and for a hash index.
CREATE TABLE ga4_pi (a int, b text);
INSERT INTO ga4_pi SELECT i, chr(65 + i % 26) FROM generate_series(1, 2000) i;
CREATE INDEX ga4_pi_part ON ga4_pi (a) WHERE b < 'B';
CREATE INDEX ga4_pi_hash ON ga4_pi USING hash (a) WHERE a = 77;
VACUUM ANALYZE ga4_pi;
EXPLAIN (COSTS OFF) SELECT a FROM ga4_pi WHERE a = 26 AND b < 'B';
EXPLAIN (COSTS OFF) SELECT a FROM ga4_pi WHERE a = 26 AND b < 'B' FOR UPDATE;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_pi WHERE a = 77;
SELECT * FROM ga4_pi WHERE a = 77;

-- Index-only scans of expression keys; ordered scans' resjunk ORDER BY keys.
CREATE TABLE ga4_e (a int, b text, p point);
INSERT INTO ga4_e SELECT i, 'v' || i, point(i, i) FROM generate_series(1, 1000) i;
CREATE INDEX ga4_e_ab ON ga4_e ((a + 1), a);
CREATE INDEX ga4_e_b ON ga4_e (b);
CREATE INDEX ga4_e_gist ON ga4_e USING gist (p, circle(p, 1));
VACUUM ANALYZE ga4_e;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT a FROM ga4_e WHERE a + 1 > 5 AND a < 7;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM ga4_e WHERE a + 1 = 5;
EXPLAIN (VERBOSE, COSTS OFF) SELECT p FROM ga4_e WHERE circle(p, 1) @> circle(point(0, 0), 0.95);
EXPLAIN (VERBOSE, COSTS OFF) SELECT p FROM ga4_e ORDER BY circle(p, 1) <-> point(0, 0) LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM ga4_e ORDER BY p <-> point(0, 0);
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM ga4_e ORDER BY b;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM ga4_e ORDER BY b LIMIT 2;
SELECT a FROM ga4_e ORDER BY b LIMIT 2;
RESET enable_seqscan;
RESET enable_bitmapscan;

-- Dummy relations: a subquery proven empty (a set-returning select list too), named.
CREATE TABLE ga4_few (id int, dataa text, datab text);
INSERT INTO ga4_few VALUES (1, 'a', 'foo'), (2, 'a', 'bar'), (3, 'b', 'bar');
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM ga4_few f1, (SELECT unnest(ARRAY[1, 2]) FROM ga4_few f2 WHERE false OFFSET 0) ss;
SELECT * FROM ga4_few f1, (SELECT unnest(ARRAY[1, 2]) FROM ga4_few f2 WHERE false OFFSET 0) ss;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM ga4_few f1 LEFT JOIN (SELECT unnest(ARRAY[1, 2]) u FROM ga4_few f2 WHERE false OFFSET 0) ss ON true;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM ga4_few f1, ga4_few f2 WHERE f2.id = 1 AND f2.id = 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM ga4_few f1 WHERE f1.id = 1 AND f1.id = 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM ga4_few f1 JOIN (SELECT * FROM ga4_few f2 WHERE false) s ON true;

-- A void SQL function whose body computes something else isn't inlined.
CREATE FUNCTION ga4_void1(a int) RETURNS void LANGUAGE sql AS $$ SELECT a + 1 $$;
CREATE FUNCTION ga4_void2(a int, b int) RETURNS void LANGUAGE sql AS $$ SELECT ga4_void1(a + b) $$;
EXPLAIN (VERBOSE, COSTS OFF) SELECT ga4_void2(11, 22);
EXPLAIN (VERBOSE, COSTS OFF) SELECT ga4_void1(11);
SELECT ga4_void2(11, 22), ga4_void1(1);

-- A sort over a domain of a column equated with a constant; declared byte-order collations.
CREATE TEMP TABLE ga4_undername (f1 name, f2 int);
CREATE TEMP VIEW ga4_overview AS SELECT f1::information_schema.sql_identifier AS sqli, f2 FROM ga4_undername;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_overview WHERE sqli = 'foo' ORDER BY sqli;
CREATE TABLE ga4_ct (a int, x text COLLATE "C", y text COLLATE "POSIX", z text, n name);
EXPLAIN (COSTS OFF) SELECT * FROM ga4_ct ORDER BY x, y;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_ct ORDER BY x DESC, y COLLATE "C" ASC NULLS FIRST;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_ct ORDER BY z, n;

-- Constraint exclusion through a virtual generated column's expression.
CREATE TABLE ga4_g33 (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL NOT NULL, CHECK (b > 10));
SET constraint_exclusion TO on;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_g33 WHERE b < 10;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_g33 WHERE b IS NULL;
RESET constraint_exclusion;

-- A hashed NOT IN costs its comparison per row: it goes ahead of costlier quals.
CREATE TABLE ga4_bl (a int);
INSERT INTO ga4_bl VALUES (3), (4);
EXPLAIN (COSTS OFF) SELECT * FROM ga4_few WHERE id % 2 = 0 AND id NOT IN (SELECT a FROM ga4_bl);
SELECT * FROM ga4_few WHERE id % 2 = 0 AND id NOT IN (SELECT a FROM ga4_bl);
