-- Several partial paths per relation (add_partial_path): a scan's parallel index scans
-- kept beside its cheapest partial path for their order, gathered by a Gather Merge as
-- they are or sorted incrementally (generate_useful_gather_paths) for a complete grouping
-- in order, a partial grouping, an ORDER BY (and create_ordered_paths' bounded sort under
-- a LIMIT), a UNION's arms merged; a scan in the order the query wants gathered only in
-- order.
CREATE TABLE a7_t (a int, b int, c int);
INSERT INTO a7_t SELECT mod(i, 10), mod(i, 10), i FROM generate_series(1, 10000) s(i);
CREATE INDEX ON a7_t (a);
ANALYZE a7_t;
BEGIN;
SET LOCAL min_parallel_table_scan_size = '1kB';
SET LOCAL min_parallel_index_scan_size = '1kB';
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL max_parallel_workers_per_gather = 2;
SET LOCAL enable_incremental_sort = off;
EXPLAIN SELECT a, b, sum(c) FROM a7_t GROUP BY 1, 2 ORDER BY 1, 2, 3 LIMIT 1;
SET LOCAL enable_incremental_sort = on;
EXPLAIN SELECT a, b, sum(c) FROM a7_t GROUP BY 1, 2 ORDER BY 1, 2, 3 LIMIT 1;
EXPLAIN SELECT a, b, sum(c) FROM a7_t GROUP BY 1, 2 ORDER BY 1, 2, 3;
EXPLAIN SELECT a, b, sum(c) FROM a7_t GROUP BY 1, 2 ORDER BY 1, 2, 3 LIMIT 50;
SELECT a, b, sum(c) FROM a7_t GROUP BY 1, 2 ORDER BY 1, 2, 3 LIMIT 3;
SET LOCAL enable_hashagg = off;
EXPLAIN SELECT * FROM a7_t UNION SELECT * FROM a7_t ORDER BY 1, 3;
SELECT * FROM (SELECT * FROM a7_t UNION SELECT * FROM a7_t ORDER BY 1, 3) s LIMIT 3;
EXPLAIN SELECT DISTINCT a, b FROM a7_t;
EXPLAIN SELECT a, b, sum(c) FROM a7_t GROUP BY 1, 2;
EXPLAIN SELECT a, sum(c) FROM a7_t GROUP BY 1;
SELECT a, sum(c) FROM a7_t GROUP BY 1 ORDER BY 1 LIMIT 3;
EXPLAIN SELECT a, c FROM a7_t ORDER BY a, c LIMIT 1;
EXPLAIN SELECT a, c FROM a7_t ORDER BY a, c LIMIT 5000;
EXPLAIN SELECT a, c FROM a7_t ORDER BY a LIMIT 10;
SET LOCAL enable_hashagg = on;
SET LOCAL enable_seqscan = off;
EXPLAIN SELECT a, c FROM a7_t ORDER BY a, c;
EXPLAIN SELECT a, c FROM a7_t ORDER BY a, c LIMIT 10;
EXPLAIN SELECT a, c FROM a7_t ORDER BY a;
SELECT a, c FROM a7_t ORDER BY a, c LIMIT 3;
COMMIT;
DROP TABLE a7_t;
