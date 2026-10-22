-- A join's partial paths of every method and orientation (a Parallel Hash Join where the
-- serial join merges, a partial nested loop with the other side outside), a base
-- relation's Gather taken by the joins above it when it beats its serial path, UNION ALL
-- arms with InitPlans gathered on their own, a ProjectSet's input sorted as the sort's
-- input target (a Result restoring the ProjectSet's order), and an IN over a windowed
-- sub-select unique-ified by its columns' distinct counts.
CREATE TABLE a7_tk (unique1 int, unique2 int, two int, hundred int, fivethous int,
  tenthous int, stringu1 name);
INSERT INTO a7_tk SELECT (i * 7919) % 10000, i, i % 2, i % 100, i % 5000,
  (i * 7) % 10000, 'AA' || i FROM generate_series(0, 9999) i;
CREATE INDEX a7_tk_unique1 ON a7_tk (unique1);
CREATE INDEX a7_tk_unique2 ON a7_tk (unique2);
CREATE INDEX a7_tk_hundred ON a7_tk (hundred);
VACUUM ANALYZE a7_tk;
BEGIN;
SET LOCAL jit = off;
SET LOCAL enable_hashagg = off;
SET LOCAL enable_seqscan = off;
SET LOCAL enable_incremental_sort = off;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL min_parallel_index_scan_size = 0;
EXPLAIN SELECT count(*) FROM a7_tk t1
  JOIN a7_tk t2 ON t1.unique1 = t2.unique2
  JOIN a7_tk t3 ON t2.unique1 = t3.unique1
  ORDER BY count(*);
SELECT count(*) FROM a7_tk t1
  JOIN a7_tk t2 ON t1.unique1 = t2.unique2
  JOIN a7_tk t3 ON t2.unique1 = t3.unique1;
EXPLAIN SELECT t2.unique1 FROM a7_tk t1 JOIN a7_tk t2 ON t1.unique1 = t2.unique2;
EXPLAIN (COSTS OFF) SELECT DISTINCT unique1,
  (SELECT t.unique1 FROM a7_tk WHERE a7_tk.unique1 = t.unique1)
  FROM a7_tk t, generate_series(1, 1000);
EXPLAIN (COSTS OFF) SELECT unique1,
  (SELECT t.unique1 FROM a7_tk WHERE a7_tk.unique1 = t.unique1)
  FROM a7_tk t, generate_series(1, 1000) ORDER BY 1, 2;
COMMIT;
BEGIN;
SET LOCAL jit = off;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL max_parallel_workers_per_gather = 4;
ALTER TABLE a7_tk SET (parallel_workers = 4);
EXPLAIN SELECT 1 FROM a7_tk t1 JOIN a7_tk t2 ON true;
EXPLAIN SELECT t1.unique1 FROM a7_tk t1 JOIN a7_tk t2 ON true;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT unnest(ARRAY[]::integer[]) + 1 AS pathkey
  FROM a7_tk t1 JOIN a7_tk t2 ON TRUE
  ORDER BY pathkey;
EXPLAIN (COSTS OFF)
SELECT unique1 FROM a7_tk WHERE fivethous =
  (SELECT unique1 FROM a7_tk WHERE fivethous = 1 LIMIT 1)
UNION ALL
SELECT unique1 FROM a7_tk WHERE fivethous =
  (SELECT unique2 FROM a7_tk WHERE fivethous = 1 LIMIT 1)
ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, two), array(SELECT generate_series(1, two))
  FROM a7_tk ORDER BY tenthous;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, two), hundred FROM a7_tk ORDER BY tenthous, hundred;
SELECT generate_series(1, two), hundred FROM a7_tk ORDER BY tenthous, hundred LIMIT 4;
ROLLBACK;
BEGIN;
SET LOCAL jit = off;
SET LOCAL max_parallel_workers_per_gather = 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, two), hundred, tenthous FROM a7_tk ORDER BY tenthous;
EXPLAIN SELECT count(*) FROM a7_tk a WHERE (unique1, two) IN
  (SELECT unique1, row_number() OVER () FROM a7_tk b);
SELECT count(*) FROM a7_tk a WHERE (unique1, two) IN
  (SELECT unique1, row_number() OVER () FROM a7_tk b);
COMMIT;
DROP TABLE a7_tk;
