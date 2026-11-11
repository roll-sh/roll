-- An equality between two expressions, each of one FROM item's columns, puts both in one
-- equivalence class (process_equivalence with neither side a Var): a merge join sorts each
-- side by its own expression. Over an inheritance tree's Append, which can't project, a
-- Result computes the sort key (prepare_sort_from_pathkeys / inject_projection_plan); a
-- partition's whole row converted to the parent's type and cast shows parenthesized twice.
CREATE TABLE em1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE em1_p1 PARTITION OF em1 FOR VALUES FROM (0) TO (250);
CREATE TABLE em1_p2 PARTITION OF em1 FOR VALUES FROM (250) TO (600);
INSERT INTO em1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE TABLE em2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE em2_p1 PARTITION OF em2 FOR VALUES FROM (0) TO (250);
CREATE TABLE em2_p2 PARTITION OF em2 FOR VALUES FROM (250) TO (600);
INSERT INTO em2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
CREATE TABLE ep1 (a int, b int);
CREATE TABLE ep2 (a int, b int);
INSERT INTO ep1 SELECT i, i % 7 FROM generate_series(1, 300) i;
INSERT INTO ep2 SELECT i % 50, i FROM generate_series(1, 200) i;
ANALYZE em1, em2, ep1, ep2;

SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b FROM em1 t1, em2 t2 WHERE t1::text = t2::text AND t1.a = t2.b ORDER BY t1.a;
SELECT t1.a, t2.b FROM em1 t1, em2 t2 WHERE t1::text = t2::text AND t1.a = t2.b ORDER BY t1.a;
EXPLAIN (COSTS OFF)
SELECT ep1.a, ep2.b FROM ep1, ep2 WHERE ep1.a + ep1.b = ep2.b + ep2.a ORDER BY ep1.a, ep2.b;
SELECT count(*), sum(ep1.a), sum(ep2.b) FROM ep1, ep2 WHERE ep1.a + ep1.b = ep2.b + ep2.a;
EXPLAIN (COSTS OFF)
SELECT ep1.a FROM ep1, ep2 WHERE ep1.a * 2 = ep2.b - 1 AND ep1.b = ep2.a;
SELECT count(*) FROM ep1, ep2 WHERE ep1.a * 2 = ep2.b - 1 AND ep1.b = ep2.a;
RESET enable_hashjoin;
RESET enable_nestloop;
EXPLAIN (COSTS OFF)
SELECT ep1.a FROM ep1, ep2 WHERE ep1.a + ep1.b = ep2.b + ep2.a;
