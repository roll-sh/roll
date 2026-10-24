-- IN over a sub-select that is a join: pulled up into the query before anything is planned
-- (pull_up_sublinks, then pull_up_simple_subquery), its relations the semi join's right
-- side; unique-ified as a join (create_unique_path over a join relation); partitioned
-- tables joined partitionwise; relations sharing an alias named in range table order.
CREATE TABLE inj1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE inj1_p1 PARTITION OF inj1 FOR VALUES FROM (0) TO (250);
CREATE TABLE inj1_p2 PARTITION OF inj1 FOR VALUES FROM (250) TO (600);
INSERT INTO inj1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE INDEX inj1_p1_a ON inj1_p1 (a);
CREATE INDEX inj1_p2_a ON inj1_p2 (a);
CREATE TABLE inj2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE inj2_p1 PARTITION OF inj2 FOR VALUES FROM (0) TO (250);
CREATE TABLE inj2_p2 PARTITION OF inj2 FOR VALUES FROM (250) TO (600);
INSERT INTO inj2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
CREATE TABLE inj3 (a int, b int, c int) PARTITION BY RANGE (((a + b) / 2));
CREATE TABLE inj3_p1 PARTITION OF inj3 FOR VALUES FROM (0) TO (250);
CREATE TABLE inj3_p2 PARTITION OF inj3 FOR VALUES FROM (250) TO (600);
INSERT INTO inj3 SELECT i, i, i % 25 FROM generate_series(0, 599, 2) i;
ANALYZE inj1, inj2, inj3;

-- Plain tables: the sub-select's join unique-ified, then a parameterized index scan.
EXPLAIN (COSTS OFF)
SELECT t1.* FROM inj1_p1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2_p1 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3_p1 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM inj1_p1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2_p1 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3_p1 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;

-- Partitioned, not partitionwise: one alias for three inheritance trees.
EXPLAIN (COSTS OFF)
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;

-- Partitionwise.
SET enable_partitionwise_join = true;
EXPLAIN (COSTS OFF)
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1, inj3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
-- Merge joins only: a semi join merging on its side's expression.
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF)
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM inj1 t1 WHERE t1.a IN (SELECT t1.b FROM inj2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM inj3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
EXPLAIN (COSTS OFF)
SELECT t1.b FROM inj2_p1 t1 WHERE t1.b IN (SELECT t1.a + 0 FROM inj3_p1 t1 WHERE t1.c = 0);
SELECT t1.b FROM inj2_p1 t1 WHERE t1.b IN (SELECT t1.a + 0 FROM inj3_p1 t1 WHERE t1.c = 0) ORDER BY 1;
RESET enable_hashjoin;
RESET enable_nestloop;
RESET enable_partitionwise_join;

-- A correlated IN over a join, and NOT IN: rows only.
SELECT count(*) FROM inj1 x WHERE x.a IN (SELECT t1.b FROM inj2 t1, inj3 t2 WHERE t1.b = t2.a AND t2.c = x.b);
SELECT count(*) FROM inj1 x WHERE x.a NOT IN (SELECT t1.b FROM inj2 t1, inj3 t2 WHERE t1.b = t2.a);
SELECT count(*) FROM inj1 x WHERE EXISTS (SELECT 1 FROM inj2 t1, inj3 t2 WHERE t1.b = t2.a AND t1.b = x.a);
