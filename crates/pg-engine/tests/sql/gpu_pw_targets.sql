-- A LEFT JOIN's ON comparison as the inner side's parameterized index condition, a
-- constant PlaceHolderVar as a Memoize key, Inner Unique through a pulled-up subquery's
-- scan, the target list of a partitionwise join computed in each child join.
BEGIN;
CREATE TEMP TABLE gpu_t (i int PRIMARY KEY);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM gpu_t t1
    LEFT JOIN (SELECT 1 AS x, * FROM gpu_t t2(i2)) t2ss ON t1.i = t2ss.i2
    LEFT JOIN gpu_t t3(i3) ON false
    LEFT JOIN gpu_t t4(i4) ON t4.i4 > t2ss.x;
EXPLAIN (COSTS OFF)
SELECT * FROM gpu_t t1 LEFT JOIN (SELECT 1 AS x, * FROM gpu_t t2(i2)) t2ss ON t1.i = t2ss.i2;
INSERT INTO gpu_t SELECT generate_series(1, 5);
SELECT * FROM gpu_t t1
    LEFT JOIN (SELECT 1 AS x, * FROM gpu_t t2(i2)) t2ss ON t1.i = t2ss.i2
    LEFT JOIN gpu_t t4(i4) ON t4.i4 > t2ss.x + 2
ORDER BY 1, 4;
ROLLBACK;

SET enable_partitionwise_join = on;
CREATE TABLE gpu_p1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE gpu_p1_p1 PARTITION OF gpu_p1 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_p1_p3 PARTITION OF gpu_p1 FOR VALUES FROM (500) TO (600);
CREATE TABLE gpu_p1_p2 PARTITION OF gpu_p1 FOR VALUES FROM (250) TO (500);
INSERT INTO gpu_p1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
ANALYZE gpu_p1;
CREATE TABLE gpu_p2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE gpu_p2_p1 PARTITION OF gpu_p2 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_p2_p2 PARTITION OF gpu_p2 FOR VALUES FROM (250) TO (500);
CREATE TABLE gpu_p2_p3 PARTITION OF gpu_p2 FOR VALUES FROM (500) TO (600);
INSERT INTO gpu_p2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
ANALYZE gpu_p2;
EXPLAIN (COSTS OFF)
SELECT t1.a + t2.a, t1.c FROM gpu_p1 t1, gpu_p2 t2 WHERE t1.a = t2.b AND t1.b = 0 ORDER BY t1.a;
SELECT t1.a + t2.a, t1.c FROM gpu_p1 t1, gpu_p2 t2 WHERE t1.a = t2.b AND t1.b = 0 ORDER BY t1.a;
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b, t1.a + t2.b FROM gpu_p1 t1 LEFT JOIN gpu_p2 t2 ON t1.a = t2.b WHERE t1.b = 0 ORDER BY 1, 2;
RESET enable_partitionwise_join;
