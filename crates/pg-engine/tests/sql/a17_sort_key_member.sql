-- A merge join input sorted on an equivalence class with two of its columns: by the one
-- the query's target list has (prepare_sort_from_pathkeys takes the first member in the
-- input's target list, the reltarget listing what is needed above first). And a
-- partitionwise join at the query's topmost scan/join relation keeps only its
-- partitionwise paths (apply_scanjoin_target_to_paths).
SET enable_partitionwise_join TO true;
CREATE TABLE a17_p1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE a17_p1_1 PARTITION OF a17_p1 FOR VALUES FROM (0) TO (250);
CREATE TABLE a17_p1_3 PARTITION OF a17_p1 FOR VALUES FROM (500) TO (600);
CREATE TABLE a17_p1_2 PARTITION OF a17_p1 FOR VALUES FROM (250) TO (500);
INSERT INTO a17_p1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE INDEX a17_p1_1_a ON a17_p1_1 (a);
CREATE INDEX a17_p1_2_a ON a17_p1_2 (a);
CREATE INDEX a17_p1_3_a ON a17_p1_3 (a);
ANALYZE a17_p1;
CREATE TABLE a17_p2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE a17_p2_1 PARTITION OF a17_p2 FOR VALUES FROM (0) TO (250);
CREATE TABLE a17_p2_2 PARTITION OF a17_p2 FOR VALUES FROM (250) TO (500);
CREATE TABLE a17_p2_3 PARTITION OF a17_p2 FOR VALUES FROM (500) TO (600);
INSERT INTO a17_p2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
ANALYZE a17_p2;
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM a17_p1 t1, a17_p2 t2 WHERE t1.a = t2.a AND t1.a = t2.b ORDER BY t1.a, t2.b;
SELECT t1.a, t1.c, t2.b, t2.c FROM a17_p1 t1, a17_p2 t2 WHERE t1.a = t2.a AND t1.a = t2.b ORDER BY t1.a, t2.b;
SET enable_partitionwise_join TO false;
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM a17_p1 t1, a17_p2 t2 WHERE t1.a = t2.a AND t1.a = t2.b ORDER BY t1.a, t2.b;
-- Only the join clause's column: sorted by it.
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.c FROM a17_p1 t1, a17_p2 t2 WHERE t1.a = t2.a AND t2.a = t2.b ORDER BY t1.a;
DROP TABLE a17_p1, a17_p2;
