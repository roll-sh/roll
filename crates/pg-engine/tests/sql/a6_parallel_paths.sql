-- Partial paths for the orderings the planner finds useful: a parallel index scan in a
-- leading part of the ORDER BY's order sorted incrementally under the Gather Merge (and
-- fully when incremental sort is off); a full sort of the cheapest partial path otherwise.
CREATE TABLE a6_pp (unique1 int, two int, four int, ten int, hundred int, thousand int,
  filler text);
INSERT INTO a6_pp SELECT i, i % 2, i % 4, i % 10, i % 100, i % 1000, repeat('x', 100)
  FROM generate_series(0, 9999) i;
CREATE INDEX a6_pp_hundred ON a6_pp (hundred);
CREATE INDEX a6_pp_unique1 ON a6_pp (unique1);
ANALYZE a6_pp;
CREATE FUNCTION a6_psv(a int) RETURNS int AS $$ BEGIN RETURN a; END; $$
  PARALLEL SAFE VOLATILE LANGUAGE plpgsql;
BEGIN;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL max_parallel_workers_per_gather = 4;
EXPLAIN (COSTS OFF)
SELECT * FROM a6_pp WHERE four = 2 ORDER BY four, hundred, a6_psv(thousand);
SET LOCAL min_parallel_index_scan_size = 0;
SET LOCAL enable_seqscan = off;
EXPLAIN (COSTS OFF)
SELECT * FROM a6_pp WHERE four = 2 ORDER BY four, hundred, a6_psv(thousand);
SET LOCAL enable_incremental_sort = off;
EXPLAIN (COSTS OFF)
SELECT * FROM a6_pp WHERE four = 2 ORDER BY four, hundred, a6_psv(thousand);
EXPLAIN (COSTS OFF)
SELECT unique1, ten FROM a6_pp ORDER BY unique1, ten;
SELECT unique1, ten FROM a6_pp ORDER BY unique1, ten LIMIT 3;
ROLLBACK;
DROP TABLE a6_pp;
DROP FUNCTION a6_psv(int);
