-- Partial aggregation and UNION ALL members: an internal-state aggregate over an anonymous
-- record isn't split (its state can't be read back); a UNION ALL arm of one table without
-- quals of its own returns only the columns read above; a partitioned table's scans under
-- a parallel-safe InitPlan stay parallel.
CREATE TABLE a7_pa (unique1 int, two int, four int, filler text);
INSERT INTO a7_pa SELECT i, i % 2, i % 4, repeat('x', 50) FROM generate_series(0, 9999) i;
ANALYZE a7_pa;
CREATE TABLE a7_lp (a int) PARTITION BY LIST (a);
CREATE TABLE a7_lp_12 PARTITION OF a7_lp FOR VALUES IN (1, 2) PARTITION BY LIST (a);
CREATE TABLE a7_lp_12_1 PARTITION OF a7_lp_12 FOR VALUES IN (1);
CREATE TABLE a7_lp_12_2 PARTITION OF a7_lp_12 FOR VALUES IN (2);
ALTER TABLE a7_lp_12_1 SET (parallel_workers = 0);
BEGIN;
SET LOCAL jit = off;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL max_parallel_workers_per_gather = 2;
EXPLAIN (COSTS OFF) SELECT array_dims(array_agg(s)) FROM (SELECT * FROM a7_pa) s;
SELECT array_dims(array_agg(s)) FROM (SELECT * FROM a7_pa) s;
EXPLAIN (COSTS OFF) SELECT array_dims(array_agg(t)) FROM a7_pa t;
EXPLAIN (COSTS OFF) SELECT array_dims(array_agg(row(t.unique1, t.two))) FROM a7_pa t;
EXPLAIN (COSTS OFF) SELECT count(s) FROM (SELECT * FROM a7_pa) s;
SET LOCAL max_parallel_workers_per_gather = 4;
SET LOCAL parallel_leader_participation = off;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT variance(unique1::int4), sum(unique1::int8)
FROM (SELECT * FROM a7_pa UNION ALL SELECT * FROM a7_pa) u;
SELECT variance(unique1::int4), sum(unique1::int8)
FROM (SELECT * FROM a7_pa UNION ALL SELECT * FROM a7_pa) u;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT count(*) FROM (SELECT * FROM a7_pa UNION ALL SELECT * FROM a7_pa) u;
EXPLAIN (COSTS OFF) SELECT * FROM a7_lp WHERE a = (SELECT 1);
COMMIT;
DROP TABLE a7_pa;
DROP TABLE a7_lp;
