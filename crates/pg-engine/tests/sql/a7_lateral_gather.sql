-- A LATERAL subquery over a function call whose select list reads the left side is
-- pulled up (the join no longer LATERAL); a nested loop over a side's partial plan
-- sorted in the query's order under a Gather Merge keeps that order for the DISTINCT or
-- ORDER BY above (expression keys computed at the relation, a value derived from a name
-- column sorted under "C").
CREATE TABLE a7_lg (unique1 int, stringu1 name, filler text);
INSERT INTO a7_lg SELECT (i * 7919) % 10000, 'AA' || i, repeat('x', 40)
  FROM generate_series(0, 9999) i;
CREATE INDEX a7_lg_unique1 ON a7_lg (unique1);
VACUUM ANALYZE a7_lg;
EXPLAIN (VERBOSE, COSTS OFF) SELECT sub.unique1, stringu1
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 3)) AS sub;
SELECT count(*), sum(sub.x) FROM a7_lg,
  LATERAL (SELECT a7_lg.unique1 + g AS x FROM generate_series(1, 3) g) AS sub;
BEGIN;
SET LOCAL jit = off;
SET LOCAL enable_hashagg = off;
SET LOCAL enable_seqscan = off;
SET LOCAL enable_incremental_sort = off;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL min_parallel_index_scan_size = 0;
EXPLAIN (COSTS OFF) SELECT DISTINCT sub.unique1, stringu1
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 1000)) AS sub;
EXPLAIN (COSTS OFF) SELECT sub.unique1, stringu1
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 1000)) AS sub
  ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT DISTINCT sub.unique1, md5(stringu1)
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 1000)) AS sub;
EXPLAIN (COSTS OFF) SELECT sub.unique1, md5(stringu1)
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 1000)) AS sub
  ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT DISTINCT sub.unique1, stringu1 || random()::text
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 1000)) AS sub;
SELECT DISTINCT sub.unique1, stringu1
  FROM a7_lg, LATERAL (SELECT a7_lg.unique1 FROM generate_series(1, 10)) AS sub
  ORDER BY 1 LIMIT 3;
COMMIT;
DROP TABLE a7_lg;
