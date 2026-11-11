-- Round 22, agent 91: partial paths in the join search. The cheapest partial path joins in
-- another order than the cheapest serial one (TPC-H q05's shape): nation x region in the
-- workers, then customer and orders by their indexes, where the serial plan hashes orders.
CREATE TABLE a91_region (r_regionkey int PRIMARY KEY, r_name text);
CREATE TABLE a91_nation (n_nationkey int PRIMARY KEY, n_name text, n_regionkey int);
CREATE TABLE a91_customer (c_custkey int PRIMARY KEY, c_nationkey int, c_pad text);
CREATE TABLE a91_orders (o_orderkey int PRIMARY KEY, o_custkey int, o_orderdate date, o_pad text);
CREATE INDEX a91_orders_cust ON a91_orders (o_custkey);
CREATE INDEX a91_orders_date ON a91_orders (o_orderdate);
CREATE INDEX a91_customer_nation ON a91_customer (c_nationkey);
INSERT INTO a91_region SELECT i, 'r' || i FROM generate_series(0, 4) i;
INSERT INTO a91_nation SELECT i, 'n' || i, i % 5 FROM generate_series(0, 24) i;
INSERT INTO a91_customer SELECT i, i % 25, repeat('x', 100) FROM generate_series(1, 15000) i;
INSERT INTO a91_orders SELECT i, 1 + (i * 7919) % 15000, date '1992-01-01' + (i % 2400), repeat('y', 60) FROM generate_series(1, 150000) i;
VACUUM ANALYZE a91_region, a91_nation, a91_customer, a91_orders;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET parallel_setup_cost = 10;
SET parallel_tuple_cost = 0.001;
EXPLAIN (COSTS OFF)
SELECT n_name, count(*) FROM a91_customer, a91_orders, a91_nation, a91_region
WHERE c_custkey = o_custkey AND c_nationkey = n_nationkey AND n_regionkey = r_regionkey
  AND r_name = 'r2' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01'
GROUP BY n_name ORDER BY 2 DESC;
SELECT n_name, count(*) FROM a91_customer, a91_orders, a91_nation, a91_region
WHERE c_custkey = o_custkey AND c_nationkey = n_nationkey AND n_regionkey = r_regionkey
  AND r_name = 'r2' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01'
GROUP BY n_name ORDER BY 2 DESC, 1;
SET max_parallel_workers_per_gather = 0;
EXPLAIN (COSTS OFF)
SELECT n_name, count(*) FROM a91_customer, a91_orders, a91_nation, a91_region
WHERE c_custkey = o_custkey AND c_nationkey = n_nationkey AND n_regionkey = r_regionkey
  AND r_name = 'r2' AND o_orderdate >= date '1994-01-01' AND o_orderdate < date '1995-01-01'
GROUP BY n_name ORDER BY 2 DESC;
RESET ALL;
DROP TABLE a91_region, a91_nation, a91_customer, a91_orders;
