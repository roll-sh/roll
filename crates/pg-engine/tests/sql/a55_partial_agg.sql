-- create_partial_grouping_paths adds the partial HashAggregate through add_partial_path
-- after the sorted partial aggregations: fuzzily (1%) as cheap as one whose order is
-- better, it goes, and the Finalize GroupAggregate reads the sorted partial groups; much
-- cheaper, it stays (sorted per worker, merged, or gathered and finalized by hashing).
CREATE TABLE a55_big (g int, k int, pad text) WITH (autovacuum_enabled = false);
INSERT INTO a55_big SELECT g, g % 7, md5(g::text) FROM generate_series(1, 200000) g;
VACUUM ANALYZE a55_big;
EXPLAIN SELECT k, count(*) FROM a55_big WHERE g % 100 = 0 GROUP BY k ORDER BY k;
EXPLAIN SELECT k, count(*) FROM a55_big WHERE g % 100 = 0 GROUP BY k;
EXPLAIN SELECT k, count(*) FROM a55_big WHERE g % 100 = 0 AND pad > 'a' GROUP BY k ORDER BY k;
EXPLAIN SELECT k, count(*), sum(g) FROM a55_big GROUP BY k ORDER BY k;
EXPLAIN SELECT k, count(*), sum(g) FROM a55_big GROUP BY k;
SET max_parallel_workers_per_gather = 4;
EXPLAIN SELECT k, count(*) FROM a55_big WHERE g % 100 = 0 GROUP BY k ORDER BY k;
RESET max_parallel_workers_per_gather;
SELECT k, count(*) FROM a55_big WHERE g % 100 = 0 GROUP BY k ORDER BY k;
SELECT k, count(*), sum(g) FROM a55_big GROUP BY k ORDER BY k;
DROP TABLE a55_big;
