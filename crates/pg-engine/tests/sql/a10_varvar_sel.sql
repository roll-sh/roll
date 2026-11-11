-- Round 15 (agent 10): a restriction comparing two columns of one relation is estimated with
-- DEFAULT_EQ_SEL (eqsel's get_restriction_variable fails for var = var), not as a join; the
-- per-partition nested loops of a partitionwise join over such derived restrictions.
CREATE COLLATION a10_vci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE TABLE a10_vs1 (a int, b int, c text collate a10_vci, d text collate a10_vci, e text, f text);
INSERT INTO a10_vs1 SELECT i % 3, i % 2, substr('abAB', (i % 4) + 1 , 1), substr('abAB', (i % 2) + 1 , 1), i::text, (i%5)::text FROM generate_series(0, 599) i;
ANALYZE a10_vs1;
EXPLAIN SELECT * FROM a10_vs1 WHERE a = b;
EXPLAIN SELECT * FROM a10_vs1 WHERE c = d;
EXPLAIN SELECT * FROM a10_vs1 WHERE e = f;
EXPLAIN SELECT * FROM a10_vs1 x JOIN a10_vs1 y ON x.a = y.b;
SET max_parallel_workers_per_gather TO 0;
SET enable_incremental_sort TO off;
SET enable_partitionwise_aggregate TO true;
SET enable_partitionwise_join TO true;
CREATE TABLE a10_vtab5 (c text collate a10_vci, b text collate a10_vci) PARTITION BY LIST (c collate a10_vci);
CREATE TABLE a10_vtab5_p1 PARTITION OF a10_vtab5 FOR VALUES IN ('a', 'b');
CREATE TABLE a10_vtab5_p2 PARTITION OF a10_vtab5 FOR VALUES IN ('c', 'd');
INSERT INTO a10_vtab5 (b, c) SELECT substr('abAB', (i % 4) + 1 , 1), substr('abAB', (i % 2) + 1 , 1) FROM generate_series(0, 5) i;
INSERT INTO a10_vtab5 (b, c) SELECT substr('cdCD', (i % 4) + 1 , 1), substr('cdCD', (i % 2) + 1 , 1) FROM generate_series(0, 5) i;
ANALYZE a10_vtab5;
CREATE TABLE a10_vtab6 (c text collate a10_vci, b text collate a10_vci) PARTITION BY LIST (b collate a10_vci);
CREATE TABLE a10_vtab6_p1 PARTITION OF a10_vtab6 FOR VALUES IN ('a', 'b');
CREATE TABLE a10_vtab6_p2 PARTITION OF a10_vtab6 FOR VALUES IN ('c', 'd');
INSERT INTO a10_vtab6 (b, c) SELECT substr('abAB', (i % 4) + 1 , 1), substr('abAB', (i % 2) + 1 , 1) FROM generate_series(0, 5) i;
INSERT INTO a10_vtab6 (b, c) SELECT substr('cdCD', (i % 4) + 1 , 1), substr('cdCD', (i % 2) + 1 , 1) FROM generate_series(0, 5) i;
ANALYZE a10_vtab6;
EXPLAIN (COSTS OFF)
SELECT t1.c, count(t2.c) FROM a10_vtab5 t1 JOIN a10_vtab6 t2 ON t1.c = t2.c AND t1.c = t2.b GROUP BY 1 ORDER BY t1.c COLLATE "C";
SELECT t1.c, count(t2.c) FROM a10_vtab5 t1 JOIN a10_vtab6 t2 ON t1.c = t2.c AND t1.c = t2.b GROUP BY 1 ORDER BY t1.c COLLATE "C";
SET enable_partitionwise_aggregate TO false;
EXPLAIN (COSTS OFF)
SELECT t1.c, count(t2.c) FROM a10_vtab5 t1 JOIN a10_vtab6 t2 ON t1.c = t2.c AND t1.c = t2.b GROUP BY 1 ORDER BY t1.c COLLATE "C";
DROP TABLE a10_vs1, a10_vtab5, a10_vtab6;
DROP COLLATION a10_vci;
