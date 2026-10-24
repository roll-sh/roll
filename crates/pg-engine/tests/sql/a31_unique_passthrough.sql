-- A semi join's right side unique-ified (create_unique_path) returns the relation's whole
-- reltarget: the HashAggregate (or Unique) groups on the semi join's operands only and
-- passes the other columns through from each group's first row. Here the IN's join puts
-- the expression `(t2.a + t2.b) / 2` in the class of `t1.a` (the semi join's clause is a
-- class's, as an inner join's), so its columns are needed above and come through, and the
-- index scan of the outer side is parameterized by that expression. In a partitionwise
-- child join the parameterized scan's loop count is the parent's rows (a child's relid is
-- in no semi join's syn_lefthand: adjust_rowcount_for_semijoins leaves it).
CREATE TABLE up1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE up1_p1 PARTITION OF up1 FOR VALUES FROM (0) TO (250);
CREATE TABLE up1_p3 PARTITION OF up1 FOR VALUES FROM (500) TO (600);
CREATE TABLE up1_p2 PARTITION OF up1 FOR VALUES FROM (250) TO (500);
INSERT INTO up1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE INDEX up1_p1_a ON up1_p1 (a);
CREATE INDEX up1_p2_a ON up1_p2 (a);
CREATE INDEX up1_p3_a ON up1_p3 (a);
CREATE TABLE up2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE up2_p1 PARTITION OF up2 FOR VALUES FROM (0) TO (250);
CREATE TABLE up2_p2 PARTITION OF up2 FOR VALUES FROM (250) TO (500);
CREATE TABLE up2_p3 PARTITION OF up2 FOR VALUES FROM (500) TO (600);
INSERT INTO up2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
CREATE INDEX up2_p1_b ON up2_p1 (b);
CREATE INDEX up2_p2_b ON up2_p2 (b);
CREATE INDEX up2_p3_b ON up2_p3 (b);
CREATE TABLE up3 (a int, b int, c int, d varchar) PARTITION BY RANGE (((a + b) / 2));
CREATE TABLE up3_p1 PARTITION OF up3 FOR VALUES FROM (0) TO (250);
CREATE TABLE up3_p2 PARTITION OF up3 FOR VALUES FROM (250) TO (500);
CREATE TABLE up3_p3 PARTITION OF up3 FOR VALUES FROM (500) TO (600);
INSERT INTO up3 SELECT i, i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599, 2) i;
CREATE INDEX up3_p1_ab2 ON up3_p1 (((a + b) / 2));
CREATE INDEX up3_p2_ab2 ON up3_p2 (((a + b) / 2));
CREATE INDEX up3_p3_ab2 ON up3_p3 (((a + b) / 2));
ANALYZE up1, up2, up3;

SET enable_partitionwise_join = on;
-- Hashed: a HashAggregate on t1_5.b passing t2's columns through.
EXPLAIN (COSTS OFF)
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a < 3 AND t1.b = (t2.a + t2.b) / 2) AND t1.b < 3 ORDER BY t1.a;
SELECT t1.a, t1.c FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a < 3 AND t1.b = (t2.a + t2.b) / 2) AND t1.b < 3 ORDER BY t1.a;
-- Sorted: a Unique over the join sorted on t1_5.b, the same columns passed through.
SET enable_hashagg = off;
EXPLAIN (COSTS OFF)
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
RESET enable_hashagg;
-- Nested semi joins: the inner one's right side returns nothing above it.
EXPLAIN (COSTS OFF)
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM up3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1 WHERE t1.b IN (SELECT (t1.a + t1.b) / 2 FROM up3 t1 WHERE t1.c = 0)) AND t1.b = 0 ORDER BY t1.a;
RESET enable_partitionwise_join;

-- Not partitionwise: the semi join's loop count is unique-ified, a hash join wins.
EXPLAIN (COSTS OFF)
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
SELECT t1.* FROM up1 t1 WHERE t1.a IN (SELECT t1.b FROM up2 t1, up3 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0 ORDER BY t1.a;
EXPLAIN (COSTS OFF) SELECT t1.* FROM up1_p1 t1 WHERE t1.a IN (SELECT t1.b FROM up2_p1 t1, up3_p1 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (COSTS OFF)
SELECT t1.* FROM up1_p1 t1 WHERE t1.a IN (SELECT t1.b FROM up2_p1 t1, up3_p1 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0;
SELECT t1.* FROM up1_p1 t1 WHERE t1.a IN (SELECT t1.b FROM up2_p1 t1, up3_p1 t2 WHERE t1.a = 0 AND t1.b = (t2.a + t2.b) / 2) AND t1.b = 0;
RESET enable_hashjoin;
RESET enable_mergejoin;

-- Semi joins' clauses in classes with an inner join's: one clause per class at each join.
CREATE TABLE sa (a int, b int, c text);
CREATE TABLE sb (a int, b int, c text);
CREATE TABLE sc (a int, b int, c text);
INSERT INTO sa SELECT i, i % 50, 'a' || i FROM generate_series(1, 1000) i;
INSERT INTO sb SELECT i * 2, i % 7, 'b' || i FROM generate_series(1, 500) i;
INSERT INTO sc SELECT i % 300, i, 'c' || i FROM generate_series(1, 2000) i;
CREATE INDEX ON sa (a);
CREATE INDEX ON sb (a);
CREATE INDEX ON sc ((a + b));
ANALYZE sa, sb, sc;
EXPLAIN (COSTS OFF) SELECT * FROM sa, sb WHERE sa.a = sb.a AND sa.a IN (SELECT a FROM sc WHERE b < 50);
SELECT count(*), sum(sa.b) FROM sa, sb WHERE sa.a = sb.a AND sa.a IN (SELECT a FROM sc WHERE b < 50);
EXPLAIN (COSTS OFF) SELECT * FROM sa WHERE sa.a IN (SELECT sb.a FROM sb, sc WHERE sb.a = sc.a + sc.b AND sc.b < 20);
SELECT count(*), sum(sa.b) FROM sa WHERE sa.a IN (SELECT sb.a FROM sb, sc WHERE sb.a = sc.a + sc.b AND sc.b < 20);
EXPLAIN (COSTS OFF) SELECT * FROM sa, sb WHERE sa.b = sb.b AND sa.a IN (SELECT sc.a FROM sc WHERE sc.b < 10) AND sb.a IN (SELECT sc.b FROM sc WHERE sc.a < 3);
SELECT count(*) FROM sa, sb WHERE sa.b = sb.b AND sa.a IN (SELECT sc.a FROM sc WHERE sc.b < 10) AND sb.a IN (SELECT sc.b FROM sc WHERE sc.a < 3);
EXPLAIN (COSTS OFF) SELECT * FROM sa WHERE sa.a IN (SELECT sb.a FROM sb WHERE sb.b IN (SELECT sc.b FROM sc WHERE sc.a = 5));
SELECT count(*) FROM sa WHERE sa.a IN (SELECT sb.a FROM sb WHERE sb.b IN (SELECT sc.b FROM sc WHERE sc.a = 5));
EXPLAIN (COSTS OFF) SELECT * FROM sa JOIN sb ON sa.a = sb.a WHERE sb.a IN (SELECT sc.a FROM sc, sb s2 WHERE sc.a = s2.a AND s2.b = 1);
SELECT count(*) FROM sa JOIN sb ON sa.a = sb.a WHERE sb.a IN (SELECT sc.a FROM sc, sb s2 WHERE sc.a = s2.a AND s2.b = 1);
EXPLAIN (COSTS OFF) SELECT * FROM sa WHERE EXISTS (SELECT 1 FROM sb, sc WHERE sb.a = sa.a AND sc.a = sb.a AND sc.b < 30);
SELECT count(*) FROM sa WHERE EXISTS (SELECT 1 FROM sb, sc WHERE sb.a = sa.a AND sc.a = sb.a AND sc.b < 30);
