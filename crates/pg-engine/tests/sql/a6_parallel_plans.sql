-- Parallel plans: SubPlans of an aggregation's target list listed before those of its
-- aggregates' arguments; an ANY over a sub-select cast to its own type; a projection of
-- values computed after the sort that workers can compute pushed under the Gather Merge;
-- each worker's sort under EXPLAIN ANALYZE; UNION ALL arms as a Parallel Append; partial
-- groupings over expression keys (their group counts by the columns, the keys referenced
-- above the partial grouping, the keys computed again per group in the cost); IN over a
-- sub-select with a window function as a semi join.
CREATE TABLE a6_big (unique1 int, two int, four int, ten int, twenty int, hundred int,
  thousand int, fivethous int, tenthous int, filler text);
INSERT INTO a6_big SELECT i, i % 2, i % 4, i % 10, i % 20, i % 100, i % 1000, i % 5000,
  (i * 7) % 10000, repeat('x', 100) FROM generate_series(0, 9999) i;
CREATE INDEX a6_big_unique1 ON a6_big (unique1);
CREATE INDEX a6_big_hundred ON a6_big (hundred);
ANALYZE a6_big;
CREATE TABLE a6_part (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE a6_part_p1 PARTITION OF a6_part FOR VALUES FROM (minvalue) TO (0);
CREATE TABLE a6_part_p2 PARTITION OF a6_part FOR VALUES FROM (0) TO (maxvalue);
BEGIN;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL max_parallel_workers_per_gather = 4;
EXPLAIN (COSTS OFF)
SELECT (SELECT max((SELECT p1.b FROM a6_part p1 WHERE p1.a = p2.a))) FROM a6_part p2;
EXPLAIN (COSTS OFF)
SELECT count(unique1) FROM a6_big
  WHERE ten = ANY ((SELECT array_agg(i) FROM generate_series(1, 100, 15) i)::int[]);
SELECT count(unique1) FROM a6_big
  WHERE hundred = ANY ((SELECT array_agg(i) FROM generate_series(1, 100, 15) i)::int[]);
CREATE FUNCTION a6_simple(v int) RETURNS int AS $$ BEGIN RETURN v + 10; END; $$
  LANGUAGE plpgsql PARALLEL SAFE;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT ten, a6_simple(ten) FROM a6_big WHERE ten < 100 ORDER BY ten;
EXPLAIN SELECT ten, a6_simple(ten) FROM a6_big WHERE ten < 100 ORDER BY ten;
EXPLAIN (COSTS OFF)
SELECT unique1 FROM a6_big WHERE fivethous = tenthous + 1
UNION ALL
SELECT unique1 FROM a6_big WHERE fivethous = tenthous + 1;
EXPLAIN (COSTS OFF) SELECT unique1 FROM a6_big WHERE fivethous = tenthous + 1;
EXPLAIN SELECT count(*) FROM a6_big GROUP BY twenty, two + 0;
SELECT count(*) FROM a6_big GROUP BY twenty, two + 0 ORDER BY 1 LIMIT 3;
CREATE FUNCTION a6_safe_volatile(a int) RETURNS int AS $$ BEGIN RETURN a; END; $$
  PARALLEL SAFE VOLATILE LANGUAGE plpgsql;
EXPLAIN SELECT count(*) FROM a6_big GROUP BY twenty, a6_safe_volatile(two);
EXPLAIN (COSTS OFF)
SELECT * FROM a6_big WHERE four = 2 ORDER BY four, hundred, a6_safe_volatile(thousand);
ROLLBACK;
CREATE FUNCTION a6_sort_stats() RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE ln text;
BEGIN
  FOR ln IN
    EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF, BUFFERS OFF)
      SELECT * FROM (SELECT ten FROM a6_big WHERE ten < 100 ORDER BY ten) ss
      RIGHT JOIN (VALUES (1), (2), (3)) v(x) ON true
  LOOP
    ln := regexp_replace(ln, 'Memory: \S*', 'Memory: xxx');
    RETURN NEXT ln;
  END LOOP;
END;
$$;
BEGIN;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL max_parallel_workers_per_gather = 4;
SET LOCAL enable_indexscan = off;
SET LOCAL enable_hashjoin = off;
SET LOCAL enable_mergejoin = off;
SET LOCAL enable_material = off;
SELECT * FROM a6_sort_stats();
ROLLBACK;
DROP FUNCTION a6_sort_stats();
SELECT count(*) FROM a6_big a WHERE unique1 IN (SELECT row_number() OVER () FROM a6_big b);
SELECT count(*) FROM a6_big a WHERE (unique1, two) IN (SELECT unique1, row_number() OVER () FROM a6_big b);
EXPLAIN SELECT count(*) FROM a6_big GROUP BY twenty, two + 0;
DROP TABLE a6_big, a6_part;
