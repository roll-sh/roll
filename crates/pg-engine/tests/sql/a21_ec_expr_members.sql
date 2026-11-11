-- Expression members of equivalence classes (add_eq_member with an expression):
-- `t1.a = (t3.a + t3.b) / 2` puts the expression in t1.a's class, so `t2.b = t1.a` derives
-- `t2.b = (t3.a + t3.b) / 2`, an index condition parameterized by t2.
SET enable_partitionwise_join = true;
CREATE TABLE ecx1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE ecx1_p1 PARTITION OF ecx1 FOR VALUES FROM (0) TO (250);
CREATE TABLE ecx1_p2 PARTITION OF ecx1 FOR VALUES FROM (250) TO (600);
INSERT INTO ecx1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE TABLE ecx2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE ecx2_p1 PARTITION OF ecx2 FOR VALUES FROM (0) TO (250);
CREATE TABLE ecx2_p2 PARTITION OF ecx2 FOR VALUES FROM (250) TO (600);
INSERT INTO ecx2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
CREATE TABLE ecx3 (a int, b int, c int) PARTITION BY RANGE (((a + b) / 2));
CREATE TABLE ecx3_p1 PARTITION OF ecx3 FOR VALUES FROM (0) TO (250);
CREATE TABLE ecx3_p2 PARTITION OF ecx3 FOR VALUES FROM (250) TO (600);
INSERT INTO ecx3 SELECT i, i, i % 25 FROM generate_series(0, 599, 2) i;
CREATE INDEX ecx3_p1_ab2 ON ecx3_p1 (((a + b) / 2));
CREATE INDEX ecx3_p2_ab2 ON ecx3_p2 (((a + b) / 2));
ANALYZE ecx1, ecx2, ecx3;

EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b, t3.a + t3.b FROM ecx1 t1, ecx2 t2, ecx3 t3
WHERE t1.a = t2.b AND t1.a = (t3.a + t3.b) / 2 AND t1.b = 0 ORDER BY t1.a;
SELECT t1.a, t2.b, t3.a + t3.b FROM ecx1 t1, ecx2 t2, ecx3 t3
WHERE t1.a = t2.b AND t1.a = (t3.a + t3.b) / 2 AND t1.b = 0 ORDER BY t1.a;

-- The same expression equated with two columns: one class of all three.
SELECT count(*), sum(t1.a), sum(t2.b) FROM ecx1 t1, ecx2 t2, ecx3 t3
WHERE t1.a = (t3.a + t3.b) / 2 AND t2.b = (t3.a + t3.b) / 2;
-- A column and an expression of one relation in a class: equal there too.
SELECT count(*) FROM ecx1 t1, ecx3 t3 WHERE t1.a = (t3.a + t3.b) / 2 AND t1.a = t3.a;
SELECT count(*) FROM ecx1 t1, ecx3 t3 WHERE t1.a = (t3.a + t3.b) / 2 AND t1.a = t3.a * 2;

SET enable_partitionwise_join = false;
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b FROM ecx1_p1 t1, ecx2_p1 t2, ecx3_p1 t3
WHERE t1.a = t2.b AND t1.a = (t3.a + t3.b) / 2 AND t1.b = 0 ORDER BY t1.a;
SELECT t1.a, t2.b FROM ecx1_p1 t1, ecx2_p1 t2, ecx3_p1 t3
WHERE t1.a = t2.b AND t1.a = (t3.a + t3.b) / 2 AND t1.b = 0 ORDER BY t1.a;
RESET enable_partitionwise_join;
