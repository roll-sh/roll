-- Found by the differential tester (tests/difftest.rs). With grouping sets, an expression
-- equal to a grouping key written as a constant, other than the select-list item the key
-- names, is no grouping column: Postgres computes it below the aggregation and reads it from
-- the group's representative row, so it is never nulled by a set, but is NULL in the empty
-- grouping set's row over no input rows (the row is all NULL there).
CREATE TABLE t (a int, b int);
INSERT INTO t VALUES (1, 10), (2, 20);
CREATE TABLE e (a int, b int);
SELECT 1 AS c0, 1 AS c2 FROM t WHERE false GROUP BY ROLLUP (t.b, 1);
SELECT 1 AS c0, 1 AS c2 FROM t GROUP BY ROLLUP (t.b, 1) ORDER BY 1, 2;
SELECT 1 AS c0, 1 AS c2, 2 AS c3 FROM e GROUP BY ROLLUP (e.b, 1);
SELECT 1 AS c2, b FROM e GROUP BY ROLLUP (b, 1);
SELECT 1 AS c0, 1 AS c2 FROM t WHERE false GROUP BY CUBE (t.b, 1);
SELECT 1, 1 FROM t WHERE false GROUP BY GROUPING SETS ((1), ());
SELECT 1 AS c0, 1+1 AS c2, 2 FROM t WHERE false GROUP BY GROUPING SETS ((1+1), ());
SELECT 1 AS c0, 1 AS c2 FROM e GROUP BY GROUPING SETS ((1), ());
SELECT 1 AS c0, 1 AS c2 FROM e GROUP BY GROUPING SETS ((1));
SELECT 1 AS c0, 1 AS c2 FROM e GROUP BY GROUPING SETS (());
SELECT 1 AS c0, 'a' AS c2, 'a'::text AS c3 FROM e GROUP BY ROLLUP(1, 2);
SELECT 1 AS c0, (SELECT 1) AS c2 FROM e GROUP BY GROUPING SETS ((1), ());
SELECT 1 AS c0, 1 AS c2, count(*) FROM e GROUP BY GROUPING SETS ((1), ());
SELECT 1 AS c0, 1 AS c2, sum(1) FROM e GROUP BY GROUPING SETS ((1), ()) ORDER BY 2;
SELECT 1 AS c0, 1 AS c2 FROM e GROUP BY GROUPING SETS ((1), (), ());
SELECT 1 AS c0, 1 AS c2, now() IS NULL FROM e GROUP BY GROUPING SETS ((1), ());
SELECT 1 AS c0, count(*) FROM e GROUP BY GROUPING SETS ((e.b), 1, ()) HAVING 1 IS DISTINCT FROM NULL;
SELECT 1 AS c0, count(*) FROM t GROUP BY GROUPING SETS ((t.b), 1, ()) HAVING 1 IS DISTINCT FROM NULL ORDER BY 1, 2;
SELECT 1 AS c0, 1 AS c1, count(*) FROM t GROUP BY GROUPING SETS ((b), 1) ORDER BY 2 DESC, 1;
SELECT 1 AS c0, 1 AS c1, GROUPING(b), GROUPING(1) FROM t GROUP BY GROUPING SETS ((b), 1, ()) ORDER BY 3, 4, 1;
SELECT 1 AS c0, 1 AS c1, GROUPING(b, 1) FROM e GROUP BY GROUPING SETS ((b), 1, ());
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 AS c0, 1 AS c2 FROM t GROUP BY ROLLUP (t.b, 1);
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 AS c0, count(*) FROM t GROUP BY GROUPING SETS ((t.b), 1) HAVING 1 IS DISTINCT FROM NULL;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 AS c0, 1 AS c2, sum(1) FROM e GROUP BY GROUPING SETS ((1), ()) ORDER BY 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 AS c0, 'a' AS c2, 'a'::text AS c3 FROM e GROUP BY ROLLUP(1, 2);
CREATE TABLE ge (a int, b int);
SELECT a, CASE WHEN a > 0 THEN abs(9) ELSE abs(9) END AS c1, count(*) FROM ge GROUP BY GROUPING SETS ((a, abs(9)), ());
INSERT INTO ge VALUES (1, 2);
SELECT a, abs(9) + 1 AS c1, count(*) FROM ge GROUP BY GROUPING SETS ((a, abs(9)), ()) ORDER BY 1, 2;
SELECT 1 AS c0, 1+1 AS c2, 2 FROM ge WHERE false GROUP BY GROUPING SETS ((1+1), ());
