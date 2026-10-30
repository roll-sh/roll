-- TPC-DS-style queries (rollups, windows, CTE reuse, set operations, top-N per group,
-- correlated averages) over the TPC-H schema. tools/bench/plancmp.py --sets tpcds.

-- name: rollup_nation_year
SELECT n_name, extract(year FROM o_orderdate) AS y, sum(o_totalprice) FROM orders JOIN customer ON c_custkey = o_custkey JOIN nation ON n_nationkey = c_nationkey WHERE o_orderdate >= '1997-01-01' GROUP BY ROLLUP (n_name, extract(year FROM o_orderdate)) ORDER BY 1, 2 LIMIT 100;
-- name: rank_within_group
SELECT * FROM (SELECT c_nationkey, c_custkey, c_acctbal, rank() OVER (PARTITION BY c_nationkey ORDER BY c_acctbal DESC) AS r FROM customer) t WHERE r <= 3 ORDER BY c_nationkey, r;
-- name: cte_reused
WITH rev AS (SELECT l_suppkey AS k, sum(l_extendedprice * (1 - l_discount)) AS r FROM lineitem WHERE l_shipdate >= '1996-01-01' AND l_shipdate < '1996-04-01' GROUP BY l_suppkey) SELECT s_suppkey, s_name, r FROM supplier, rev WHERE s_suppkey = k AND r > (SELECT avg(r) * 1.2 FROM rev) ORDER BY r DESC LIMIT 20;
-- name: intersect_customers
SELECT o_custkey FROM orders WHERE o_orderdate BETWEEN '1995-01-01' AND '1995-01-31' INTERSECT SELECT o_custkey FROM orders WHERE o_orderdate BETWEEN '1996-01-01' AND '1996-01-31' ORDER BY 1 LIMIT 100;
-- name: except_parts
SELECT ps_partkey FROM partsupp WHERE ps_availqty < 20 EXCEPT SELECT l_partkey FROM lineitem WHERE l_shipdate > '1998-11-15' ORDER BY 1 LIMIT 50;
-- name: case_pivot
SELECT o_orderpriority, sum(CASE WHEN o_orderstatus = 'F' THEN 1 ELSE 0 END) AS f, sum(CASE WHEN o_orderstatus = 'O' THEN 1 ELSE 0 END) AS o, sum(CASE WHEN o_orderstatus = 'P' THEN 1 ELSE 0 END) AS p FROM orders WHERE o_orderdate >= '1996-01-01' AND o_orderdate < '1997-01-01' GROUP BY o_orderpriority ORDER BY o_orderpriority;
-- name: correlated_avg
SELECT p_partkey, p_name, p_retailprice FROM part p1 WHERE p_size = 20 AND p_retailprice > (SELECT avg(p_retailprice) * 1.3 FROM part p2 WHERE p2.p_brand = p1.p_brand) ORDER BY p_retailprice DESC LIMIT 20;
-- name: moving_avg
SELECT o_orderdate, sum(o_totalprice) AS d, avg(sum(o_totalprice)) OVER (ORDER BY o_orderdate ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) AS ma FROM orders WHERE o_orderdate BETWEEN '1997-01-01' AND '1997-03-31' GROUP BY o_orderdate ORDER BY o_orderdate;
-- name: share_of_total
SELECT n_name, sum(c_acctbal) AS s, sum(c_acctbal) / sum(sum(c_acctbal)) OVER () AS share FROM customer JOIN nation ON n_nationkey = c_nationkey GROUP BY n_name ORDER BY s DESC;
-- name: grouping_sets_ship
SELECT l_shipmode, l_returnflag, count(*), sum(l_quantity) FROM lineitem WHERE l_shipdate BETWEEN '1995-06-01' AND '1995-06-30' GROUP BY GROUPING SETS ((l_shipmode), (l_returnflag), ()) ORDER BY 1, 2;
-- name: top_customers_by_year
SELECT y, c_name, total FROM (SELECT extract(year FROM o_orderdate) AS y, o_custkey, sum(o_totalprice) AS total, row_number() OVER (PARTITION BY extract(year FROM o_orderdate) ORDER BY sum(o_totalprice) DESC) AS rn FROM orders GROUP BY 1, 2) t JOIN customer ON c_custkey = o_custkey WHERE rn = 1 ORDER BY y;
-- name: not_exists_returns
SELECT count(*) FROM orders o WHERE o_orderdate >= '1998-06-01' AND NOT EXISTS (SELECT 1 FROM lineitem l WHERE l.l_orderkey = o.o_orderkey AND l.l_returnflag = 'R');
-- name: semi_join_in_dates
SELECT s_name, count(*) FROM supplier JOIN lineitem ON l_suppkey = s_suppkey WHERE l_orderkey IN (SELECT o_orderkey FROM orders WHERE o_orderdate = '1995-03-10') GROUP BY s_name ORDER BY 2 DESC, 1 LIMIT 10;
-- name: distinct_on_latest
SELECT DISTINCT ON (o_custkey) o_custkey, o_orderkey, o_orderdate FROM orders WHERE o_custkey < 2000 ORDER BY o_custkey, o_orderdate DESC;
-- name: union_channels
SELECT 'store' AS ch, l_shipmode AS k, sum(l_quantity) AS q FROM lineitem WHERE l_shipdate BETWEEN '1997-01-01' AND '1997-01-15' GROUP BY l_shipmode UNION ALL SELECT 'web', o_orderpriority, count(*) FROM orders WHERE o_orderdate BETWEEN '1997-01-01' AND '1997-01-15' GROUP BY o_orderpriority ORDER BY 1, 2;
-- name: ntile_buckets
SELECT b, count(*), min(o_totalprice), max(o_totalprice) FROM (SELECT o_totalprice, ntile(4) OVER (ORDER BY o_totalprice) AS b FROM orders WHERE o_orderdate BETWEEN '1994-01-01' AND '1994-01-31') t GROUP BY b ORDER BY b;
-- name: scalar_subquery_select
SELECT n_name, (SELECT count(*) FROM supplier WHERE s_nationkey = n_nationkey) AS sups, (SELECT max(c_acctbal) FROM customer WHERE c_nationkey = n_nationkey) AS best FROM nation ORDER BY n_name;
-- name: left_join_agg_filter
SELECT p_brand, count(ps_suppkey) FILTER (WHERE ps_supplycost < 100) AS cheap, count(*) FROM part LEFT JOIN partsupp ON ps_partkey = p_partkey WHERE p_size BETWEEN 1 AND 3 GROUP BY p_brand ORDER BY p_brand;
