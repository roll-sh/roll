-- Partitionwise LATERAL joins of two items: the LATERAL side's clauses equate its
-- partition keys with the other side's, so each partition joins its counterpart (a nested
-- loop per partition pair, the LATERAL references the partition's), level by level for
-- sub-partitioned tables; a LEFT JOIN keeps every outer row.
SET enable_partitionwise_join TO true;
CREATE TABLE a12_p1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE a12_p1_1 PARTITION OF a12_p1 FOR VALUES FROM (0) TO (250);
CREATE TABLE a12_p1_3 PARTITION OF a12_p1 FOR VALUES FROM (500) TO (600);
CREATE TABLE a12_p1_2 PARTITION OF a12_p1 FOR VALUES FROM (250) TO (500);
INSERT INTO a12_p1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
ANALYZE a12_p1;
CREATE TABLE a12_p2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE a12_p2_1 PARTITION OF a12_p2 FOR VALUES FROM (0) TO (250);
CREATE TABLE a12_p2_2 PARTITION OF a12_p2 FOR VALUES FROM (250) TO (500);
CREATE TABLE a12_p2_3 PARTITION OF a12_p2 FOR VALUES FROM (500) TO (600);
INSERT INTO a12_p2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
ANALYZE a12_p2;
EXPLAIN (COSTS OFF)
SELECT * FROM a12_p1 t1 JOIN LATERAL
  (SELECT * FROM a12_p1 t2 TABLESAMPLE SYSTEM (t1.a) REPEATABLE (t1.b)) s ON t1.a = s.a;
SELECT count(*) FROM a12_p1 t1 JOIN LATERAL
  (SELECT * FROM a12_p1 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s ON t1.a = s.a;
EXPLAIN (COSTS OFF)
SELECT t1.a, s.b FROM a12_p1 t1 LEFT JOIN LATERAL
  (SELECT * FROM a12_p2 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s ON t1.a = s.b
  WHERE t1.a % 25 = 0 ORDER BY 1, 2;
SELECT t1.a, s.b FROM a12_p1 t1 LEFT JOIN LATERAL
  (SELECT * FROM a12_p2 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s ON t1.a = s.b
  WHERE t1.a % 25 = 0 ORDER BY 1, 2;
-- Not on the partition key: no partitionwise join.
SELECT count(*) FROM a12_p1 t1 JOIN LATERAL
  (SELECT * FROM a12_p2 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s ON t1.a = s.a;
-- Pruned partitions: the inner join of what both sides keep.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a12_p1 t1 JOIN LATERAL
  (SELECT * FROM a12_p2 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b) WHERE t2.b < 400) s ON t1.a = s.b
  WHERE t1.a >= 200;
SELECT count(*) FROM a12_p1 t1 JOIN LATERAL
  (SELECT * FROM a12_p2 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b) WHERE t2.b < 400) s ON t1.a = s.b
  WHERE t1.a >= 200;
-- A LEFT JOIN whose inner side lost a partition: not partitionwise (null-extended rows).
SELECT count(*), count(s.b) FROM a12_p1 t1 LEFT JOIN LATERAL
  (SELECT * FROM a12_p2 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b) WHERE t2.b < 400) s ON t1.a = s.b;
-- A LATERAL relation reading the outer side's values: its parameterized paths are costed for
-- the outer rows as loops (an index scan per partition).
CREATE INDEX a12_p2_1_b ON a12_p2_1 (b);
CREATE INDEX a12_p2_2_b ON a12_p2_2 (b);
CREATE INDEX a12_p2_3_b ON a12_p2_3 (b);
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a12_p1 t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a12_p2 t2) s ON t1.a = s.b WHERE s.t1b = s.a;
SELECT count(*) FROM a12_p1 t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a12_p2 t2) s ON t1.a = s.b WHERE s.t1b = s.a;
SELECT count(*), count(s.b) FROM a12_p1 t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a12_p2 t2) s ON t1.a = s.b;
-- Sub-partitioned, every level's key equated.
CREATE TABLE a12_l1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE a12_l1_1 PARTITION OF a12_l1 FOR VALUES FROM (0) TO (250);
CREATE TABLE a12_l1_2 PARTITION OF a12_l1 FOR VALUES FROM (250) TO (500) PARTITION BY LIST (c);
CREATE TABLE a12_l1_2_1 PARTITION OF a12_l1_2 FOR VALUES IN ('0000', '0001');
CREATE TABLE a12_l1_2_2 PARTITION OF a12_l1_2 FOR VALUES IN ('0002', '0003');
CREATE TABLE a12_l1_3 PARTITION OF a12_l1 FOR VALUES FROM (500) TO (600) PARTITION BY RANGE (b);
CREATE TABLE a12_l1_3_1 PARTITION OF a12_l1_3 FOR VALUES FROM (0) TO (13);
CREATE TABLE a12_l1_3_2 PARTITION OF a12_l1_3 FOR VALUES FROM (13) TO (25);
INSERT INTO a12_l1 SELECT i, i % 25, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 2) i;
ANALYZE a12_l1;
EXPLAIN (COSTS OFF)
SELECT * FROM a12_l1 t1 JOIN LATERAL
  (SELECT * FROM a12_l1 t2 TABLESAMPLE SYSTEM (t1.a) REPEATABLE (t1.b)) s
  ON t1.a = s.a AND t1.b = s.b AND t1.c = s.c;
SELECT count(*) FROM a12_l1 t1 JOIN LATERAL
  (SELECT * FROM a12_l1 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s
  ON t1.a = s.a AND t1.b = s.b AND t1.c = s.c;
-- Only the top level's key: the sub-partitioned pairs joined as one.
SELECT count(*) FROM a12_l1 t1 JOIN LATERAL
  (SELECT * FROM a12_l1 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s
  ON t1.a = s.a;
-- Off: the plain nested loop.
SET enable_partitionwise_join TO false;
SELECT count(*) FROM a12_l1 t1 JOIN LATERAL
  (SELECT * FROM a12_l1 t2 TABLESAMPLE SYSTEM (100) REPEATABLE (t1.b)) s
  ON t1.a = s.a AND t1.b = s.b AND t1.c = s.c;
RESET enable_partitionwise_join;
