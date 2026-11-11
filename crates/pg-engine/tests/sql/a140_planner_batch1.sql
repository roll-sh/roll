-- Round 24, agent 140, batch 1: a grouping over a join reads the join relation's paths sorted
-- on some group keys (pathkeys_useful_for_grouping, add_paths_to_grouping_rel); an uncorrelated
-- sub-select bounds an index scan (an InitPlan's Param, a runtime key); a KNN ordering written
-- the other way round commutes to the index's operator. Recorded from PostgreSQL 18.6.
CREATE TABLE a140_cust (c_custkey int PRIMARY KEY, c_name text, c_nationkey int, c_acctbal numeric);
CREATE TABLE a140_ord (o_orderkey int PRIMARY KEY, o_custkey int, o_totalprice numeric, o_orderdate date);
CREATE TABLE a140_line (l_orderkey int, l_linenumber int, l_quantity numeric, l_extendedprice numeric, PRIMARY KEY (l_orderkey, l_linenumber));
INSERT INTO a140_cust SELECT g, 'Customer#' || g, g % 25, (g * 7919) % 10000 FROM generate_series(1, 6000) g;
INSERT INTO a140_ord SELECT g, 1 + (g * 7) % 6000, (g * 31) % 50000, date '1992-01-01' + (g % 2400) FROM generate_series(1, 60000) g;
INSERT INTO a140_line SELECT o, l, 1 + (o * 13 + l * 7) % 50, ((o * 13 + l * 7) % 50) * 10.5 FROM generate_series(1, 60000) o, generate_series(1, 4) l;
VACUUM ANALYZE a140_cust, a140_ord, a140_line;
SET max_parallel_workers_per_gather = 0;
-- A grouping without ORDER BY: no sorted path to read, hashed.
EXPLAIN (COSTS OFF) SELECT c_custkey, count(*) FROM a140_cust, a140_ord
WHERE c_custkey = o_custkey AND o_orderdate < date '1992-02-01'
GROUP BY c_custkey, c_name;
SET work_mem = '64kB';
-- TPC-H q18's shape: the group keys reduced to the primary keys (remove_useless_groupby_columns),
-- the merge join in c_custkey order kept for them, an Incremental Sort for the rest.
EXPLAIN (COSTS OFF) SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum(l_quantity)
FROM a140_cust, a140_ord, a140_line
WHERE o_orderkey IN (SELECT l_orderkey FROM a140_line GROUP BY l_orderkey HAVING sum(l_quantity) > 150)
  AND c_custkey = o_custkey AND o_orderkey = l_orderkey
GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice
ORDER BY o_totalprice DESC, o_orderdate LIMIT 10;
SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum(l_quantity)
FROM a140_cust, a140_ord, a140_line
WHERE o_orderkey IN (SELECT l_orderkey FROM a140_line GROUP BY l_orderkey HAVING sum(l_quantity) > 150)
  AND c_custkey = o_custkey AND o_orderkey = l_orderkey
GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice
ORDER BY o_totalprice DESC, o_orderdate LIMIT 10;
-- TPC-H q10's: the sorted join path covers the grouping.
EXPLAIN (COSTS OFF) SELECT c_custkey, c_name, sum(l_extendedprice) AS revenue, c_acctbal
FROM a140_cust, a140_ord, a140_line
WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey
  AND o_orderdate >= date '1993-10-01' AND o_orderdate < date '1994-01-01'
GROUP BY c_custkey, c_name, c_acctbal
ORDER BY revenue DESC LIMIT 20;
SELECT c_custkey, c_name, sum(l_extendedprice) AS revenue, c_acctbal
FROM a140_cust, a140_ord, a140_line
WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey
  AND o_orderdate >= date '1993-10-01' AND o_orderdate < date '1994-01-01'
GROUP BY c_custkey, c_name, c_acctbal
ORDER BY revenue DESC, c_custkey LIMIT 20;
RESET work_mem;
RESET max_parallel_workers_per_gather;
-- An uncorrelated sub-select as an index scan's key: the InitPlan's value bounds the scan.
DELETE FROM a140_ord WHERE o_orderkey <= 30000;
EXPLAIN (COSTS OFF) DELETE FROM a140_ord WHERE o_orderkey = (SELECT min(o_orderkey) FROM a140_ord);
DELETE FROM a140_ord WHERE o_orderkey = (SELECT min(o_orderkey) FROM a140_ord) RETURNING o_orderkey;
DELETE FROM a140_ord WHERE o_orderkey = (SELECT min(o_orderkey) FROM a140_ord) RETURNING o_orderkey;
SELECT o_orderkey FROM a140_ord WHERE o_orderkey >= (SELECT 59998) ORDER BY o_orderkey;
SELECT o_orderkey FROM a140_ord WHERE o_orderkey < (SELECT 30005) ORDER BY o_orderkey DESC;
SELECT count(*) FROM a140_ord WHERE o_orderkey = (SELECT NULL::int);
SELECT count(*) FROM a140_ord WHERE o_orderkey = ANY (ARRAY(SELECT g FROM generate_series(30010, 30012) g));
DROP TABLE a140_cust, a140_ord, a140_line;
-- KNN ordering written with the column on the right: commuted to the index's operator
-- (match_clause_to_ordering_op).
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE TABLE a140_trgm (t text);
INSERT INTO a140_trgm SELECT md5(g::text) FROM generate_series(1, 2000) g;
CREATE INDEX a140_trgm_idx ON a140_trgm USING gist (t gist_trgm_ops);
ANALYZE a140_trgm;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT t FROM a140_trgm ORDER BY 'c4ca42' <<-> t LIMIT 3;
SELECT t FROM a140_trgm ORDER BY 'c4ca42' <<-> t, t LIMIT 3;
EXPLAIN (COSTS OFF) SELECT t FROM a140_trgm ORDER BY 'c4ca42' <<<-> t LIMIT 3;
RESET enable_seqscan;
DROP TABLE a140_trgm;
DROP EXTENSION pg_trgm;
