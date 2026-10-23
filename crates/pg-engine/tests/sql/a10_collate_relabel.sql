-- Round 15 (agent 10): a written COLLATE over a column of another collation is a RelabelType
-- in the plan: group keys and join clauses show `(c)::text`, references to it wrapped, the
-- sort above `((c)::text) COLLATE "C"`; ANALYZE sorts and groups a column's values under its
-- collation (pg_stats' MCVs and histogram, the join estimates over them).
CREATE COLLATION a10_rci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
SET max_parallel_workers_per_gather TO 0;
SET enable_incremental_sort TO off;
CREATE TABLE a10_rtab (a text, c text collate a10_rci) PARTITION BY LIST(c collate "C");
CREATE TABLE a10_rtab_p1 PARTITION OF a10_rtab FOR VALUES IN ('a', 'b');
CREATE TABLE a10_rtab_p2 PARTITION OF a10_rtab FOR VALUES IN ('B', 'A');
INSERT INTO a10_rtab SELECT i % 4 + 1, substr('abAB', (i % 4) + 1 , 1) FROM generate_series(0, 19) i;
ANALYZE a10_rtab;
SET enable_partitionwise_aggregate TO true;
EXPLAIN (COSTS OFF)
SELECT c collate "C", count(c) FROM a10_rtab GROUP BY c collate "C" ORDER BY 1;
SELECT c collate "C", count(c) FROM a10_rtab GROUP BY c collate "C" ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT c collate "C", count(c) FROM a10_rtab GROUP BY c collate "C" ORDER BY 1;
SET enable_partitionwise_join TO true;
EXPLAIN (COSTS OFF)
SELECT t1.c COLLATE "C", count(t2.c) FROM a10_rtab t1 JOIN a10_rtab t2 ON t1.c = t2.c COLLATE "C" GROUP BY t1.c COLLATE "C" ORDER BY t1.c COLLATE "C";
SET enable_partitionwise_join TO false;
EXPLAIN (COSTS OFF)
SELECT t1.c COLLATE "C", count(t2.c) FROM a10_rtab t1 JOIN a10_rtab t2 ON t1.c = t2.c COLLATE "C" GROUP BY t1.c COLLATE "C" ORDER BY t1.c COLLATE "C";
SELECT t1.c COLLATE "C", count(t2.c) FROM a10_rtab t1 JOIN a10_rtab t2 ON t1.c = t2.c COLLATE "C" GROUP BY t1.c COLLATE "C" ORDER BY t1.c COLLATE "C";
CREATE TABLE a10_d (c text);
INSERT INTO a10_d VALUES ('a'), ('b');
EXPLAIN (VERBOSE, COSTS OFF) SELECT c COLLATE "C", count(*) FROM a10_d GROUP BY c COLLATE "C" ORDER BY 1;
CREATE COLLATION a10_ren (provider = icu, locale = 'en');
CREATE TABLE a10_st1 (c text collate a10_rci, e text collate a10_ren, d text);
INSERT INTO a10_st1 SELECT substr('abAB', (i % 4) + 1 , 1), substr('abAB', (i % 4) + 1 , 1) || i % 3, substr('abAB', (i % 4) + 1 , 1) FROM generate_series(0, 19) i;
ANALYZE a10_st1;
SELECT attname, n_distinct, most_common_vals, most_common_freqs, histogram_bounds, correlation FROM pg_stats WHERE tablename = 'a10_st1' ORDER BY attname;
DROP TABLE a10_rtab, a10_d, a10_st1;
DROP COLLATION a10_rci;
DROP COLLATION a10_ren;
