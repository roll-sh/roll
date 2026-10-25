-- Hash aggregation's group order: the hash table sized as PostgreSQL 18 sizes it (the
-- input's width, the transition states, half of what fits the memory limit), and a
-- sorted GroupAggregate's groups in its sort order, its aggregates over the sorted rows.
DROP TABLE IF EXISTS a48_hao;
CREATE TABLE a48_hao (id int, k int, x float8, y numeric(12, 2), s text, n numeric);
INSERT INTO a48_hao SELECT i, (i * 7919) % 30000, i / 7.0, (i % 1000) / 10.0, md5(i::text), (i % 5)::numeric(4, 1)
  FROM generate_series(1, 30000) i;
ANALYZE a48_hao;
SET max_parallel_workers_per_gather = 0;
-- Hashed, the table capped by work_mem.
SET work_mem = '256kB';
EXPLAIN (COSTS OFF) SELECT k % 50, count(*) FROM a48_hao GROUP BY 1;
SELECT k % 50, count(*) FROM a48_hao GROUP BY 1;
SELECT k % 50, count(*), sum(x) FROM a48_hao GROUP BY 1;
SELECT k % 50, count(*), avg(y), max(s) FROM a48_hao GROUP BY 1;
SELECT (k % 40)::text, count(*), avg(y) FROM a48_hao GROUP BY 1;
RESET work_mem;
SELECT k % 50, count(*), sum(x) FROM a48_hao GROUP BY 1;
SELECT k % 50, count(*), avg(y), max(s) FROM a48_hao GROUP BY 1;
-- Sorted (a GroupAggregate over a Sort): groups in key order, float sums and array_agg in
-- the sort's row order (the sort in memory).
SET work_mem = '64kB';
EXPLAIN (COSTS OFF) SELECT k % 50, count(*) FROM a48_hao GROUP BY 1;
SELECT k % 50, count(*) FROM a48_hao GROUP BY 1;
SELECT (k % 40)::numeric, count(*), min(id) FROM a48_hao GROUP BY 1;
RESET work_mem;
SET enable_hashagg = off;
SELECT k % 40, count(*), sum(x), avg(x) FROM a48_hao GROUP BY 1;
SELECT k % 40, string_agg(s, ',') FROM a48_hao WHERE id < 200 GROUP BY 1;
SELECT k % 40, array_agg(id) FROM a48_hao WHERE id < 300 GROUP BY 1;
SELECT k % 40, max(n), min(n) FROM a48_hao GROUP BY 1;
SELECT k % 40 FROM a48_hao GROUP BY 1;
SELECT k % 40 AS a, n, count(*) FROM a48_hao GROUP BY n, a;
SELECT DISTINCT k % 40 FROM a48_hao;
RESET enable_hashagg;
-- DISTINCT aggregates (counted without sorting where the order can't show).
SELECT count(DISTINCT k % 777), count(DISTINCT x), sum(DISTINCT k % 100), avg(DISTINCT y) FROM a48_hao;
SELECT k % 7, count(DISTINCT n), count(DISTINCT s), sum(DISTINCT n), avg(DISTINCT k % 10) FROM a48_hao GROUP BY 1 ORDER BY 1;
SELECT count(DISTINCT NULLIF(k % 5, 0)), count(DISTINCT (k % 3)::numeric(5, 2)) FROM a48_hao;
-- Sorts on numeric keys, ties between rows that differ.
SELECT n, id FROM a48_hao WHERE id < 40 ORDER BY n;
SELECT n, id FROM a48_hao WHERE id < 40 ORDER BY n DESC LIMIT 7;
SELECT y, id FROM a48_hao WHERE id % 1000 < 3 ORDER BY y DESC NULLS LAST, id;
SELECT x::numeric(10, 1) v, id FROM a48_hao WHERE id < 30 ORDER BY v DESC;
SELECT id % 4 g, id FROM a48_hao WHERE id < 25 ORDER BY g;
DROP TABLE a48_hao;
