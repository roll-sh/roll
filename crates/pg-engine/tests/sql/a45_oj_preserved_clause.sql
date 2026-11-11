-- Found by the differential tester (tests/difftest.rs). An outer join's ON clause that reads
-- only its preserved side is checked at the join: a subquery planned apart on that side has
-- to return the column it reads. The join search took the clause for one of that side's
-- restrictions, so the subquery returned NULL for the column and the clause never held.
CREATE TABLE t (x int);
CREATE TABLE u (y int);
INSERT INTO t VALUES (1), (2), (-3);
INSERT INTO u VALUES (10), (20);
SELECT * FROM (SELECT x, x > 0 AS p FROM t OFFSET 0) s LEFT JOIN u ON s.p ORDER BY 1, 3;
SELECT * FROM u RIGHT JOIN (SELECT x, x > 0 AS p FROM t OFFSET 0) s ON s.p ORDER BY 2, 1;
SELECT * FROM (VALUES (1), (2)) a(x) RIGHT JOIN (SELECT count(*) > -1 AS c FROM (SELECT true AS x WHERE false) s) v ON v.c ORDER BY 1;
SELECT * FROM (VALUES (1), (2)) a(x) RIGHT JOIN (SELECT count(*) > -1 AS c FROM (SELECT true AS x WHERE false) s) v ON v.c AND a.x > 1;
SELECT * FROM (SELECT x, sum(x) OVER () > 0 AS p FROM t) s LEFT JOIN u ON s.p AND u.y > 10 ORDER BY 1, 3;
SELECT * FROM (SELECT DISTINCT x, x < 2 AS p FROM t) s LEFT JOIN u ON s.p ORDER BY 1, 3;
SELECT * FROM (SELECT x, x > 0 AS p FROM t OFFSET 0) s LEFT JOIN u ON s.p JOIN u u2 ON u2.y = 10 ORDER BY 1, 3;
SELECT * FROM (SELECT x, x > 0 AS p FROM t OFFSET 0) s LEFT JOIN (u JOIN u u2 ON u.y = u2.y) ON s.p ORDER BY 1, 3;
