-- Round 15 (agent 10): equivalence classes under a nondeterministic collation (a scan's
-- members equated there, one join clause per class, kept apart from other collations'),
-- partitionwise joins matching partition keys through such classes only under their
-- collation, full partitionwise aggregation by a key of the partition key's collation, a
-- hashed grouping kept as the cheapest path when a sorted one is only fuzzily as cheap.
CREATE COLLATION a10_ci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE COLLATION a10_cs (provider = icu, locale = '');
CREATE TABLE a10_a (c text COLLATE a10_ci);
CREATE TABLE a10_b (c text COLLATE a10_ci, b text COLLATE a10_ci);
INSERT INTO a10_a SELECT substr('abAB', (i % 4) + 1 , 1) FROM generate_series(0, 19) i;
INSERT INTO a10_b SELECT substr('abAB', (i % 4) + 1 , 1), substr('abAB', (i % 2) + 1 , 1) FROM generate_series(0, 11) i;
ANALYZE a10_a;
ANALYZE a10_b;
EXPLAIN (COSTS OFF) SELECT * FROM a10_a JOIN a10_b ON a10_a.c = a10_b.c AND a10_a.c = a10_b.b;
SELECT a10_a.c COLLATE "C", count(*) FROM a10_a JOIN a10_b ON a10_a.c = a10_b.c AND a10_a.c = a10_b.b
  GROUP BY 1 ORDER BY 1;
-- (Another collation's equality is a class of its own.)
SELECT count(*) FROM a10_a JOIN a10_b ON a10_a.c = a10_b.c COLLATE "C" AND a10_a.c = a10_b.b;

SET max_parallel_workers_per_gather TO 0;
SET enable_incremental_sort TO off;
SET enable_partitionwise_join TO true;
SET enable_partitionwise_aggregate TO true;
CREATE TABLE a10_p3 (a text, c text collate a10_ci) PARTITION BY LIST(c collate "C");
CREATE TABLE a10_p3_1 PARTITION OF a10_p3 FOR VALUES IN ('a', 'b');
CREATE TABLE a10_p3_2 PARTITION OF a10_p3 FOR VALUES IN ('B', 'A');
INSERT INTO a10_p3 SELECT i % 4 + 1, substr('abAB', (i % 4) + 1 , 1) FROM generate_series(0, 19) i;
ANALYZE a10_p3;
CREATE TABLE a10_p4 (c text collate a10_ci, b text collate a10_ci) PARTITION BY LIST (b collate "C");
CREATE TABLE a10_p4_1 PARTITION OF a10_p4 FOR VALUES IN ('a', 'b');
CREATE TABLE a10_p4_2 PARTITION OF a10_p4 FOR VALUES IN ('B', 'A');
INSERT INTO a10_p4 (b, c) SELECT substr('abAB', (i % 4) + 1 , 1), substr('abAB', (i % 2) + 1 , 1) FROM generate_series(0, 11) i;
ANALYZE a10_p4;
-- The partition keys are equated only under a10_ci: not partitionwise (rows of
-- different partitions match).
EXPLAIN (COSTS OFF)
SELECT t1.c, count(t2.c) FROM a10_p3 t1 JOIN a10_p4 t2 ON t1.c = t2.c AND t1.c = t2.b GROUP BY 1 ORDER BY t1.c COLLATE "C";
SELECT t1.c, count(t2.c) FROM a10_p3 t1 JOIN a10_p4 t2 ON t1.c = t2.c AND t1.c = t2.b GROUP BY 1 ORDER BY t1.c COLLATE "C";
-- GROUP BY under the partition key's collation: full partitionwise aggregation.
SELECT c collate "C", count(c) FROM a10_p3 GROUP BY c collate "C" ORDER BY 1;
-- But not under the column's own.
EXPLAIN (COSTS OFF)
SELECT c, count(c) FROM a10_p3 GROUP BY c ORDER BY c COLLATE "C";
SELECT c, count(c) FROM a10_p3 GROUP BY c ORDER BY c COLLATE "C";
RESET enable_partitionwise_aggregate;
RESET enable_partitionwise_join;
RESET enable_incremental_sort;
RESET max_parallel_workers_per_gather;

CREATE TABLE a10_t1 (x text COLLATE a10_cs);
CREATE TABLE a10_t3 (x text COLLATE a10_cs);
INSERT INTO a10_t1 VALUES ('abc'), ('def'), ('ghi');
INSERT INTO a10_t3 VALUES ('abc'), ('ABC'), ('def'), ('ghi');
CREATE UNIQUE INDEX ON a10_t3 (x);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a10_t1 t1, (SELECT x FROM a10_t3 GROUP BY x) t2
WHERE t1.x = t2.x COLLATE a10_ci
ORDER BY 1, 2;
SELECT * FROM a10_t1 t1, (SELECT x FROM a10_t3 GROUP BY x) t2
WHERE t1.x = t2.x COLLATE a10_ci
ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT x FROM a10_t3 GROUP BY x;
EXPLAIN (COSTS OFF) SELECT x FROM a10_t3 GROUP BY x ORDER BY x;
DROP TABLE a10_t1, a10_t3, a10_p3, a10_p4, a10_a, a10_b;
DROP COLLATION a10_ci;
DROP COLLATION a10_cs;
