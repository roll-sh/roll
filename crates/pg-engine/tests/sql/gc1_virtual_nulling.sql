-- Virtual generated columns on the nullable side of outer joins (and under grouping
-- sets) go NULL with the row: constants and non-strict expressions included.
CREATE TABLE gvn (a int PRIMARY KEY, b int GENERATED ALWAYS AS (a * 2), c int GENERATED ALWAYS AS (10 + 10), d int GENERATED ALWAYS AS (coalesce(a, 100)), e int);
INSERT INTO gvn (a, e) VALUES (1, 5), (2, NULL);
CREATE TABLE gvn_o (x int);
INSERT INTO gvn_o VALUES (1), (3);
SELECT o.x, t.* FROM gvn_o o LEFT JOIN gvn t ON t.a = o.x ORDER BY o.x;
SELECT o.x, t.* FROM gvn t RIGHT JOIN gvn_o o ON t.a = o.x ORDER BY o.x;
SELECT o.x, t.a, t.c, t.d FROM gvn_o o FULL JOIN gvn t ON t.a = o.x ORDER BY o.x, t.a;
SELECT t2.* FROM gvn t1 LEFT JOIN gvn t2 ON false ORDER BY t1.a;
SELECT o.x, s.c, s.d FROM gvn_o o LEFT JOIN LATERAL (SELECT * FROM gvn t WHERE t.a = o.x) s ON true ORDER BY o.x;
SELECT o.x, s.c FROM gvn_o o LEFT JOIN (SELECT a, c, d FROM gvn) s ON s.a = o.x ORDER BY o.x;
CREATE VIEW gvn_v AS SELECT * FROM gvn;
SELECT o.x, v.c, v.d FROM gvn_o o LEFT JOIN gvn_v v ON v.a = o.x ORDER BY o.x;
SELECT t.c, count(*), sum(t.d) FROM gvn_o o LEFT JOIN gvn t ON t.a = o.x GROUP BY t.c ORDER BY t.c;
SELECT count(t.c), count(t.d), count(t.b) FROM gvn_o o LEFT JOIN gvn t ON t.a = o.x;
SELECT * FROM gvn GROUP BY GROUPING SETS (a, b, c, d, e) HAVING c = 20;
SELECT c, d, count(*) FROM gvn GROUP BY ROLLUP (c, d) ORDER BY c, d;
EXPLAIN (COSTS OFF) SELECT o.x, t.c FROM gvn_o o LEFT JOIN gvn t ON t.a = o.x;
