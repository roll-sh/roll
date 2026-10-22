-- Join planning details: GROUP BY () without aggregates is degenerate grouping (no input
-- read), an ANY SubPlan's test proves its left side non-null (join strength reduction), a
-- semi join's clause with the subquery's constant fixes the outer column, inequality join
-- clauses parameterize an appendrel's index scans (pruned as each outer row comes), an
-- inner Materialize is rescanned per outer row, partitionwise child joins keep the
-- parent join's equivalence classes (redundant GROUP BY keys go), a COALESCE equated with a
-- constant above a left join fixes the next join's inner side.
CREATE TABLE a5_pb (id int PRIMARY KEY) PARTITION BY RANGE (id);
CREATE TABLE a5_pb1 PARTITION OF a5_pb FOR VALUES FROM (0) TO (10);
INSERT INTO a5_pb VALUES (1), (2);
EXPLAIN (COSTS OFF)
SELECT 1 FROM (SELECT t1.id FROM a5_pb t1 LEFT JOIN a5_pb t2 ON t1.id = t2.id) s WHERE s.id = 1 GROUP BY ();
SELECT 1 FROM (SELECT t1.id FROM a5_pb t1 LEFT JOIN a5_pb t2 ON t1.id = t2.id) s WHERE s.id = 1 GROUP BY ();
EXPLAIN (COSTS OFF) SELECT 1 FROM a5_pb GROUP BY (), ();
SELECT 1 FROM a5_pb GROUP BY (), ();
EXPLAIN (COSTS OFF) SELECT 1 FROM a5_pb HAVING 1 > 2;
SELECT 1 FROM a5_pb HAVING 1 > 2;
SELECT 1 FROM a5_pb WHERE false GROUP BY ();
CREATE TABLE a5_onek (unique1 int, unique2 int);
INSERT INTO a5_onek SELECT g, 999 - g FROM generate_series(0, 999) g;
CREATE UNIQUE INDEX a5_onek_u1 ON a5_onek (unique1);
CREATE TABLE a5_i8 (q1 int8, q2 int8);
INSERT INTO a5_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123);
ANALYZE a5_onek;
ANALYZE a5_i8;
EXPLAIN (COSTS OFF)
SELECT a.unique1, b.unique2
  FROM a5_onek a LEFT JOIN a5_onek b ON a.unique1 = b.unique2
  WHERE (b.unique2, random() > 0) = ANY (SELECT q1, random() > 0 FROM a5_i8 c WHERE c.q1 < b.unique1);
SELECT a.unique1, b.unique2
  FROM a5_onek a LEFT JOIN a5_onek b ON a.unique1 = b.unique2
  WHERE (b.unique2, random() > 0) = ANY (SELECT q1, random() > 0 FROM a5_i8 c WHERE c.q1 < b.unique1);
CREATE TABLE a5_t (a int, b int);
INSERT INTO a5_t VALUES (1, 2), (1, 3), (2, 2);
ANALYZE a5_t;
EXPLAIN (COSTS OFF)
SELECT * FROM a5_t t1, a5_t t2 WHERE EXISTS
  (SELECT 1 FROM a5_t t3 WHERE t1.a = t3.a AND t2.b = t3.b AND t3.a = 1 AND t3.b = 2);
SELECT * FROM a5_t t1, a5_t t2 WHERE EXISTS
  (SELECT 1 FROM a5_t t3 WHERE t1.a = t3.a AND t2.b = t3.b AND t3.a = 1 AND t3.b = 2) ORDER BY 1, 2, 3, 4;
CREATE TABLE a5_tbl1 (col1 int);
INSERT INTO a5_tbl1 VALUES (501), (505);
CREATE TABLE a5_tprt (col1 int) PARTITION BY RANGE (col1);
CREATE TABLE a5_tprt_1 PARTITION OF a5_tprt FOR VALUES FROM (1) TO (501);
CREATE TABLE a5_tprt_2 PARTITION OF a5_tprt FOR VALUES FROM (501) TO (1001);
CREATE TABLE a5_tprt_3 PARTITION OF a5_tprt FOR VALUES FROM (1001) TO (2001);
CREATE INDEX ON a5_tprt_1 (col1);
CREATE INDEX ON a5_tprt_2 (col1);
CREATE INDEX ON a5_tprt_3 (col1);
INSERT INTO a5_tprt VALUES (10), (20), (501), (502), (505), (1001);
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_indexonlyscan = off;
EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF, BUFFERS OFF)
SELECT * FROM a5_tbl1 JOIN a5_tprt ON a5_tbl1.col1 > a5_tprt.col1;
EXPLAIN (COSTS OFF) SELECT * FROM a5_tbl1 LEFT JOIN a5_tprt ON a5_tbl1.col1 < a5_tprt.col1;
SELECT a5_tbl1.col1, a5_tprt.col1 FROM a5_tbl1 JOIN a5_tprt ON a5_tbl1.col1 > a5_tprt.col1 ORDER BY 1, 2;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_indexonlyscan;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_indexscan = off;
SET enable_bitmapscan = off;
EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF, BUFFERS OFF)
SELECT * FROM a5_t t1 JOIN a5_t t2 ON t1.a < t2.b;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_indexscan;
RESET enable_bitmapscan;
SET enable_partitionwise_join = on;
CREATE TABLE a5_p1 (a int, b int, c text) PARTITION BY LIST (c);
CREATE TABLE a5_p1_x PARTITION OF a5_p1 FOR VALUES IN ('0000', '0001');
CREATE TABLE a5_p1_y PARTITION OF a5_p1 FOR VALUES IN ('0002', '0003');
CREATE TABLE a5_p2 (a int, b int, c text) PARTITION BY LIST (c);
CREATE TABLE a5_p2_x PARTITION OF a5_p2 FOR VALUES IN ('0000', '0001');
CREATE TABLE a5_p2_y PARTITION OF a5_p2 FOR VALUES IN ('0002', '0003');
INSERT INTO a5_p1 SELECT i, i % 25, to_char(i % 4, 'FM0000') FROM generate_series(0, 299) i;
INSERT INTO a5_p2 SELECT i, i % 25, to_char(i % 4, 'FM0000') FROM generate_series(0, 299, 3) i;
ANALYZE a5_p1;
ANALYZE a5_p2;
EXPLAIN (COSTS OFF)
SELECT avg(t1.a), avg(t2.b), t1.c, t2.c FROM a5_p1 t1, a5_p2 t2
  WHERE t1.b = t2.b AND t1.c = t2.c GROUP BY t1.c, t2.c ORDER BY t1.c, t2.c;
SELECT avg(t1.a), avg(t2.b), t1.c, t2.c FROM a5_p1 t1, a5_p2 t2
  WHERE t1.b = t2.b AND t1.c = t2.c GROUP BY t1.c, t2.c ORDER BY t1.c, t2.c;
RESET enable_partitionwise_join;
CREATE TABLE a5_tk (unique1 int, unique2 int, thousand int, twothousand int);
INSERT INTO a5_tk SELECT g, g, g % 1000, g % 2000 FROM generate_series(0, 9999) g;
CREATE INDEX ON a5_tk (unique2);
CREATE INDEX ON a5_tk (thousand);
ANALYZE a5_tk;
EXPLAIN (COSTS OFF)
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM a5_tk a LEFT JOIN a5_tk b ON b.thousand = a.unique1
  LEFT JOIN a5_tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 44;
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM a5_tk a LEFT JOIN a5_tk b ON b.thousand = a.unique1
  LEFT JOIN a5_tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 44;
DROP TABLE a5_pb, a5_onek, a5_i8, a5_t, a5_tbl1, a5_tprt, a5_p1, a5_p2, a5_tk;
