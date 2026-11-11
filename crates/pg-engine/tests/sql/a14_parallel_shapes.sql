-- Round 16 (agent 14): parallel plan shapes. A LATERAL join's outer relation read through
-- its Gather (generate_useful_gather_paths at the base relation); a partial DISTINCT over an
-- ordered partial index scan with its keys reordered (get_useful_pathkeys_for_distinct); an
-- outer join's partial paths either way round, also when the serial plan is a merge join;
-- hash tables sized as ExecChooseHashTableSize does (batches halved only while that saves
-- memory, a Parallel Hash's combined hash_mem) and their batch counts under EXPLAIN ANALYZE.
CREATE TABLE a14_l1 AS SELECT i AS unique1, i % 2 AS two, repeat('x', 200) AS pad FROM generate_series(1, 10000) i;
CREATE TABLE a14_l2 AS SELECT i AS unique1, i % 2 AS two, repeat('y', 200) AS pad FROM generate_series(1, 10000) i;
ANALYZE a14_l1;
ANALYZE a14_l2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 4;
ALTER TABLE a14_l2 SET (parallel_workers = 0);
EXPLAIN (COSTS OFF)
SELECT * FROM a14_l1 t1 LEFT JOIN LATERAL (SELECT t1.unique1 AS x, * FROM a14_l2 t2 ORDER BY 1) t2 ON true
WHERE t1.two > t2.two;
ALTER TABLE a14_l2 RESET (parallel_workers);
RESET max_parallel_workers_per_gather;
RESET min_parallel_table_scan_size;
RESET parallel_tuple_cost;
RESET parallel_setup_cost;
-- A partial DISTINCT over a parallel index-only scan in the keys' other order.
CREATE TABLE a14_distinct (x int, y int);
INSERT INTO a14_distinct SELECT i % 10, i % 10 FROM generate_series(1, 1000) AS i;
CREATE INDEX a14_distinct_x_y ON a14_distinct (x, y);
ANALYZE a14_distinct;
SET enable_hashagg = off;
SET parallel_tuple_cost = 0;
SET parallel_setup_cost = 0;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
EXPLAIN SELECT DISTINCT y, x FROM a14_distinct LIMIT 10;
SELECT DISTINCT y, x FROM a14_distinct LIMIT 10;
EXPLAIN SELECT DISTINCT y, x FROM a14_distinct;
SET enable_seqscan = off;
EXPLAIN SELECT DISTINCT y, x FROM a14_distinct LIMIT 10;
RESET enable_seqscan;
RESET max_parallel_workers_per_gather;
RESET min_parallel_index_scan_size;
RESET min_parallel_table_scan_size;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET enable_hashagg;
-- Hash joins: batches and outer joins' partial paths.
CREATE FUNCTION a14_find_hash(node json) RETURNS json LANGUAGE plpgsql AS $$
DECLARE x json; child json;
BEGIN
  IF node->>'Node Type' = 'Hash' THEN RETURN node; END IF;
  FOR child IN SELECT json_array_elements(node->'Plans') LOOP
    x := a14_find_hash(child);
    IF x IS NOT NULL THEN RETURN x; END IF;
  END LOOP;
  RETURN NULL;
END $$;
CREATE FUNCTION a14_batches(query text) RETURNS TABLE (original int, final int) LANGUAGE plpgsql AS $$
DECLARE whole_plan json; hash_node json;
BEGIN
  FOR whole_plan IN EXECUTE 'explain (analyze, format ''json'') ' || query LOOP
    hash_node := a14_find_hash(json_extract_path(whole_plan, '0', 'Plan'));
    original := hash_node->>'Original Hash Batches';
    final := hash_node->>'Hash Batches';
    RETURN NEXT;
  END LOOP;
END $$;
CREATE TABLE a14_simple AS SELECT generate_series(1, 20000) AS id, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
ALTER TABLE a14_simple SET (parallel_workers = 2);
ANALYZE a14_simple;
CREATE TABLE a14_wide AS SELECT generate_series(1, 2) AS id, rpad('', 320000, 'x') AS t;
ALTER TABLE a14_wide SET (parallel_workers = 2);
BEGIN;
SET LOCAL min_parallel_table_scan_size = 0;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL max_parallel_workers_per_gather = 0;
SET LOCAL work_mem = '128kB';
SET LOCAL hash_mem_multiplier = 1.0;
EXPLAIN SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id);
SELECT * FROM a14_batches($$ SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id) $$);
SET LOCAL max_parallel_workers_per_gather = 2;
SET LOCAL enable_parallel_hash = off;
SELECT * FROM a14_batches($$ SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id) $$);
SET LOCAL work_mem = '192kB';
SET LOCAL enable_parallel_hash = on;
EXPLAIN SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id);
SELECT * FROM a14_batches($$ SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id) $$);
SET LOCAL work_mem = '4MB';
EXPLAIN SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id);
SELECT * FROM a14_batches($$ SELECT count(*) FROM a14_simple r JOIN a14_simple s USING (id) $$);
SET LOCAL work_mem = '128kB';
EXPLAIN (COSTS OFF)
  SELECT length(max(s.t)) FROM a14_wide LEFT JOIN (SELECT id, coalesce(t, '') || '' AS t FROM a14_wide) s USING (id);
SELECT length(max(s.t)) FROM a14_wide LEFT JOIN (SELECT id, coalesce(t, '') || '' AS t FROM a14_wide) s USING (id);
SELECT final > 1 AS multibatch FROM a14_batches($$
  SELECT length(max(s.t)) FROM a14_wide LEFT JOIN (SELECT id, coalesce(t, '') || '' AS t FROM a14_wide) s USING (id) $$);
SET LOCAL enable_mergejoin = off;
EXPLAIN (COSTS OFF)
  SELECT length(max(s.t)) FROM a14_wide LEFT JOIN (SELECT id, coalesce(t, '') || '' AS t FROM a14_wide) s USING (id);
SET LOCAL enable_parallel_hash = off;
EXPLAIN (COSTS OFF)
  SELECT length(max(s.t)) FROM a14_wide LEFT JOIN (SELECT id, coalesce(t, '') || '' AS t FROM a14_wide) s USING (id);
ROLLBACK;
DROP TABLE a14_l1, a14_l2, a14_distinct, a14_simple, a14_wide;
DROP FUNCTION a14_batches(text);
DROP FUNCTION a14_find_hash(json);
