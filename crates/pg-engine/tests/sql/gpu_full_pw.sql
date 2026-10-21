-- Partitionwise FULL JOINs over several tables: a full join's partition key includes its
-- sides' keys merged (COALESCE), so a USING column of it joins the next table partitionwise.
SET enable_partitionwise_join = on;
CREATE TABLE gpu_f1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE gpu_f1_p1 PARTITION OF gpu_f1 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_f1_p3 PARTITION OF gpu_f1 FOR VALUES FROM (500) TO (600);
CREATE TABLE gpu_f1_p2 PARTITION OF gpu_f1 FOR VALUES FROM (250) TO (500);
INSERT INTO gpu_f1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
ANALYZE gpu_f1;
CREATE TABLE gpu_f2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE gpu_f2_p1 PARTITION OF gpu_f2 FOR VALUES FROM (0) TO (250);
CREATE TABLE gpu_f2_p2 PARTITION OF gpu_f2 FOR VALUES FROM (250) TO (500);
CREATE TABLE gpu_f2_p3 PARTITION OF gpu_f2 FOR VALUES FROM (500) TO (600);
INSERT INTO gpu_f2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
ANALYZE gpu_f2;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gpu_f1 FULL JOIN gpu_f2 p2(b, a, c) USING (a, b) FULL JOIN gpu_f2 p3(b, a, c) USING (a, b)
  WHERE a BETWEEN 490 AND 510;
SELECT count(*) FROM gpu_f1 FULL JOIN gpu_f2 p2(b, a, c) USING (a, b) FULL JOIN gpu_f2 p3(b, a, c) USING (a, b)
  WHERE a BETWEEN 490 AND 510;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gpu_f1 FULL JOIN gpu_f2 p2(b, a, c) USING (a, b) FULL JOIN gpu_f2 p3(b, a, c) USING (a, b)
  FULL JOIN gpu_f1 p4 (a, b, c) USING (a, b)
  WHERE a BETWEEN 490 AND 510;
SELECT count(*) FROM gpu_f1 FULL JOIN gpu_f2 p2(b, a, c) USING (a, b) FULL JOIN gpu_f2 p3(b, a, c) USING (a, b)
  FULL JOIN gpu_f1 p4 (a, b, c) USING (a, b)
  WHERE a BETWEEN 490 AND 510;
SELECT a, b, gpu_f1.c, p2.c, p3.c FROM gpu_f1 FULL JOIN gpu_f2 p2(b, a, c) USING (a, b)
  FULL JOIN gpu_f2 p3(b, a, c) USING (a, b)
  WHERE a BETWEEN 495 AND 505 ORDER BY 1, 2;
RESET enable_partitionwise_join;
