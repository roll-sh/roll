-- Partitionwise joins of multi-level partitioned tables: the top-level partitions joined
-- pairwise, a partition that is partitioned itself joining its partitions as one (or
-- partitionwise again when the clauses cover its key); the join clauses name its first
-- partition; restrictions its partitions have aren't added twice.
SET enable_partitionwise_join = on;
CREATE TABLE gpu_m1 (a int, b int, c varchar) PARTITION BY RANGE(a);
CREATE TABLE gpu_m1_p1 PARTITION OF gpu_m1 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_m1_p2 PARTITION OF gpu_m1 FOR VALUES FROM (250) TO (500) PARTITION BY LIST (c);
CREATE TABLE gpu_m1_p2_p1 PARTITION OF gpu_m1_p2 FOR VALUES IN ('0000', '0001');
CREATE TABLE gpu_m1_p2_p2 PARTITION OF gpu_m1_p2 FOR VALUES IN ('0002', '0003');
CREATE TABLE gpu_m1_p3 PARTITION OF gpu_m1 FOR VALUES FROM (500) TO (600) PARTITION BY RANGE (b);
CREATE TABLE gpu_m1_p3_p1 PARTITION OF gpu_m1_p3 FOR VALUES FROM (0) TO (13);
CREATE TABLE gpu_m1_p3_p2 PARTITION OF gpu_m1_p3 FOR VALUES FROM (13) TO (25);
INSERT INTO gpu_m1 SELECT i, i % 25, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 2) i;
ANALYZE gpu_m1;

CREATE TABLE gpu_m2 (a int, b int, c varchar) PARTITION BY RANGE(b);
CREATE TABLE gpu_m2_p1 PARTITION OF gpu_m2 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_m2_p2 PARTITION OF gpu_m2 FOR VALUES FROM (250) TO (500) PARTITION BY LIST (c);
CREATE TABLE gpu_m2_p2_p1 PARTITION OF gpu_m2_p2 FOR VALUES IN ('0000', '0001');
CREATE TABLE gpu_m2_p2_p2 PARTITION OF gpu_m2_p2 FOR VALUES IN ('0002', '0003');
CREATE TABLE gpu_m2_p3 PARTITION OF gpu_m2 FOR VALUES FROM (500) TO (600) PARTITION BY RANGE (a);
CREATE TABLE gpu_m2_p3_p1 PARTITION OF gpu_m2_p3 FOR VALUES FROM (0) TO (13);
CREATE TABLE gpu_m2_p3_p2 PARTITION OF gpu_m2_p3 FOR VALUES FROM (13) TO (25);
INSERT INTO gpu_m2 SELECT i % 25, i, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 3) i;
ANALYZE gpu_m2;
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1, gpu_m2 t2 WHERE t1.a = t2.b AND t1.b = 0 ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1, gpu_m2 t2 WHERE t1.a = t2.b AND t1.b = 0 ORDER BY t1.a, t2.b;

EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1, gpu_m2 t2 WHERE t1.a = t2.a AND t1.a = t2.b AND t1.c = t2.c ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1, gpu_m2 t2 WHERE t1.a = t2.a AND t1.a = t2.b AND t1.c = t2.c ORDER BY t1.a, t2.b;

EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1 LEFT JOIN gpu_m2 t2 ON t1.a = t2.b AND t1.c = t2.c WHERE t1.b = 0 ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1 LEFT JOIN gpu_m2 t2 ON t1.a = t2.b AND t1.c = t2.c WHERE t1.b = 0 ORDER BY t1.a, t2.b;

EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1 RIGHT JOIN gpu_m2 t2 ON t1.a = t2.b AND t1.c = t2.c WHERE t2.a = 0 ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM gpu_m1 t1 RIGHT JOIN gpu_m2 t2 ON t1.a = t2.b AND t1.c = t2.c WHERE t2.a = 0 ORDER BY t1.a, t2.b;

EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM (SELECT * FROM gpu_m1 WHERE gpu_m1.b = 0) t1 FULL JOIN (SELECT * FROM gpu_m2 WHERE gpu_m2.a = 0) t2 ON (t1.a = t2.b AND t1.c = t2.c) ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM (SELECT * FROM gpu_m1 WHERE gpu_m1.b = 0) t1 FULL JOIN (SELECT * FROM gpu_m2 WHERE gpu_m2.a = 0) t2 ON (t1.a = t2.b AND t1.c = t2.c) ORDER BY t1.a, t2.b;
RESET enable_partitionwise_join;
