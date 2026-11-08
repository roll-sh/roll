-- Gather transmits only the independent initplans read by its worker part. Serial
-- execution and an empty ANY right side still leave ordinary row expressions lazy.
CREATE TABLE a189_t (id int, v int, s text, d date);
INSERT INTO a189_t SELECT i, i % 100, md5(i::text), date '2000-01-01' + i % 3000 FROM generate_series(1, 200000) i;
CREATE TABLE a189_e (id int, v int, s text);
ANALYZE a189_t;
SELECT count(*) FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e);
SELECT count(*) FROM a189_t WHERE 1 / (id - id) = ANY (SELECT id FROM a189_e);
SELECT count(*) FROM a189_e WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e);
SELECT NULL::int = ANY (SELECT id FROM a189_e) AS any_empty, NULL::int <> ALL (SELECT id FROM a189_e) AS all_empty;
SELECT id FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e) LIMIT 0;
SELECT (SELECT 1 / (min(id) - min(id)) FROM a189_t) FROM a189_t WHERE id < 0;
-- The same startup boundary also covers the earlier documented filter-under-Gather gap.
SELECT count(*) FROM a189_t WHERE v < 0 AND v > (SELECT 1 / (min(v) - min(v)) FROM a189_t);
SET max_parallel_workers_per_gather=0;
SELECT count(*) FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e);
SELECT count(*) FROM a189_t WHERE v < 0 AND v > (SELECT 1 / (min(v) - min(v)) FROM a189_t);
RESET max_parallel_workers_per_gather;
-- Each execution owns its startup work, including after an exception.
PREPARE a189_start(int) AS SELECT count(*) FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id) + $1) FROM a189_t) = ANY (SELECT id FROM a189_e);
SET plan_cache_mode=force_generic_plan;
EXECUTE a189_start(1);
EXECUTE a189_start(0);
EXECUTE a189_start(1);
SET plan_cache_mode=force_custom_plan;
EXECUTE a189_start(1);
EXECUTE a189_start(0);
EXECUTE a189_start(1);
DEALLOCATE a189_start;
RESET plan_cache_mode;
BEGIN;
SELECT count(*) FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e);
SELECT 42;
ROLLBACK;
SELECT 42;
-- EXPLAIN ANALYZE uses the same boundary, directly and through SPI.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT count(*) FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e);
DO $$ BEGIN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT count(*) FROM a189_t WHERE id + (SELECT 1 / (min(id) - min(id)) FROM a189_t) = ANY (SELECT id FROM a189_e)'; END $$;
DROP TABLE a189_t, a189_e;
