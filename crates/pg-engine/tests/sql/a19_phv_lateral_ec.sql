-- A LATERAL subquery's output reading the outer row, under a LEFT JOIN a WHERE reduces to
-- inner: a PlaceHolderVar the scan evaluates. Its equality with a column the ON clause
-- already equated is a restriction the equivalence class gives back as
-- `column = PlaceHolderVar` (the earlier member first), ahead of the join clauses.
CREATE TABLE a19_l (a int, b int, c varchar);
CREATE TABLE a19_m (a int, b int, c varchar);
INSERT INTO a19_l SELECT i, i % 25, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 2) i;
INSERT INTO a19_m SELECT i % 25, i, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 3) i;
CREATE INDEX a19_m_b ON a19_m (b);
ANALYZE a19_l;
ANALYZE a19_m;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.b AND t1.b = s.a AND t1.c = s.c
  WHERE s.t1b = s.a;
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.b AND t1.b = s.a AND t1.c = s.c
  WHERE s.t1b = s.a;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.b WHERE s.t1b = s.b;
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.b WHERE s.t1b = s.b;
-- The column not in a class before: the clause as written.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.a WHERE s.t1b = s.b;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.a WHERE s.b = s.t1b;
SELECT count(*) FROM a19_l t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_m t2) s
  ON t1.a = s.a WHERE s.b = s.t1b;
-- Partitioned, partitionwise.
SET enable_partitionwise_join TO true;
CREATE TABLE a19_p1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE a19_p1_1 PARTITION OF a19_p1 FOR VALUES FROM (0) TO (250);
CREATE TABLE a19_p1_2 PARTITION OF a19_p1 FOR VALUES FROM (250) TO (600);
INSERT INTO a19_p1 SELECT i, i % 25, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 2) i;
CREATE TABLE a19_p2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE a19_p2_1 PARTITION OF a19_p2 FOR VALUES FROM (0) TO (250);
CREATE TABLE a19_p2_2 PARTITION OF a19_p2 FOR VALUES FROM (250) TO (600);
INSERT INTO a19_p2 SELECT i % 25, i, to_char(i % 4, 'FM0000') FROM generate_series(0, 599, 3) i;
CREATE INDEX a19_p2_b ON a19_p2 (b);
ANALYZE a19_p1;
ANALYZE a19_p2;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a19_p1 t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_p2 t2) s
  ON t1.a = s.b WHERE s.t1b = s.b;
SELECT count(*) FROM a19_p1 t1 LEFT JOIN LATERAL
  (SELECT t1.b AS t1b, t2.* FROM a19_p2 t2) s
  ON t1.a = s.b WHERE s.t1b = s.b;
RESET enable_partitionwise_join;
