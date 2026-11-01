-- Round 24, agent 140, batch 2: a grouping under a LIMIT reads its join's cheapest-to-start
-- sorted path (consider_startup); a CREATE FUNCTION set-returning function in the select list
-- returns its ROWS per call at its COST. Recorded from PostgreSQL 18.6.
CREATE TABLE a140b_cust (c_custkey int PRIMARY KEY, c_name text, c_nationkey int);
CREATE TABLE a140b_ord (o_orderkey int PRIMARY KEY, o_custkey int, o_orderdate date);
INSERT INTO a140b_cust SELECT g, 'Customer#' || g, g % 25 FROM generate_series(1, 6000) g;
INSERT INTO a140b_ord SELECT g, 1 + (g * 7) % 6000, date '1992-01-01' + (g % 2400) FROM generate_series(1, 60000) g;
VACUUM ANALYZE a140b_cust, a140b_ord;
SET max_parallel_workers_per_gather = 0;
EXPLAIN (COSTS OFF) SELECT c_custkey, count(*) FROM a140b_cust, a140b_ord
WHERE c_custkey = o_custkey AND o_orderdate < date '1992-02-01'
GROUP BY c_custkey, c_name LIMIT 8;
-- Without the LIMIT: the cheapest in total.
EXPLAIN (COSTS OFF) SELECT c_custkey, count(*) FROM a140b_cust, a140b_ord
WHERE c_custkey = o_custkey AND o_orderdate < date '1992-02-01'
GROUP BY c_custkey, c_name;
RESET max_parallel_workers_per_gather;
DROP TABLE a140b_cust, a140b_ord;
CREATE FUNCTION a140b_srf() RETURNS SETOF int LANGUAGE sql AS 'SELECT 1 UNION ALL SELECT 2';
CREATE FUNCTION a140b_srf2(n int) RETURNS SETOF int LANGUAGE sql ROWS 10 AS 'SELECT generate_series(1, n)';
CREATE FUNCTION a140b_srf3(n int) RETURNS SETOF int LANGUAGE sql STABLE COST 7 AS 'SELECT g FROM generate_series(1, n) g';
CREATE TABLE a140b_t (a int);
INSERT INTO a140b_t SELECT generate_series(1, 100);
ANALYZE a140b_t;
EXPLAIN SELECT a140b_srf();
EXPLAIN SELECT a140b_srf2(3);
EXPLAIN SELECT a, a140b_srf2(a) FROM a140b_t;
EXPLAIN SELECT a140b_srf3(5);
EXPLAIN SELECT a, a140b_srf() FROM a140b_t;
EXPLAIN SELECT a140b_srf(), generate_series(1, 3);
SELECT a140b_srf(), a140b_srf2(3);
DROP TABLE a140b_t;
DROP FUNCTION a140b_srf(), a140b_srf2(int), a140b_srf3(int);
