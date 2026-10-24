-- Found by the differential tester (tests/difftest.rs). A grouping set nulls a key without
-- columns only in the select-list item the key names (by position, or written alike):
-- the same constant elsewhere, an expression over it, or HAVING over it stays as is.
-- Keys with columns, volatile keys and (in the select list) outer references are nulled
-- wherever they appear.
CREATE TABLE t2 (id int4 PRIMARY KEY, a text, b text);
INSERT INTO t2 (id, a, b) VALUES (17, 'aa', 'z'), (18, 'ab', 'y');
SELECT 1 AS c0, t1.b FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1);
SELECT 1 AS c0, 1 AS c1, count(*) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1) ORDER BY 1, 2, 3;
SELECT 1 AS c0, count(*) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1) HAVING 1 IS DISTINCT FROM NULL;
SELECT 1 AS c0, count(*) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1) ORDER BY 1, 2;
SELECT 1 AS c0, count(*) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1) ORDER BY 1 DESC, 2;
SELECT 1::int AS c0, 1::int AS c1, count(*) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1::int) ORDER BY 1, 2, 3;
SELECT 1 AS c0, b, count(*), rank() OVER (PARTITION BY 1 ORDER BY b) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1) ORDER BY 1, 2, 3;
SELECT 1 + 1 AS c0, 1 + 1 AS c1, count(*) FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1 + 1) ORDER BY 1, 2, 3;
SELECT 2 AS c0, count(*) FROM t2 t1 GROUP BY ROLLUP (t1.b, 1) HAVING 2 = 2 ORDER BY 1, 2;
SELECT DISTINCT ON (1) 1 AS c0, b FROM t2 t1 GROUP BY GROUPING SETS ((t1.b), 1) ORDER BY 1, 2;
SELECT 5 AS x, count(*) FROM t2 GROUP BY x HAVING 5 > 4;
SELECT (1+1) + 0 AS c0, count(*) FROM t2 GROUP BY GROUPING SETS ((b), 1+1) ORDER BY 1, 2;
SELECT COALESCE(1+1, 5) AS c0, count(*) FROM t2 GROUP BY GROUPING SETS ((b), 1+1) ORDER BY 1, 2;
SELECT length(a) + 0 AS c0, count(*) FROM t2 GROUP BY GROUPING SETS ((b), length(a)) ORDER BY 1, 2;
SELECT COALESCE(length(a), 7) AS c0, count(*) FROM t2 GROUP BY GROUPING SETS ((b), length(a)) ORDER BY 1, 2;
SELECT count(*) FROM t2 GROUP BY GROUPING SETS ((b), 1+1) HAVING (1+1) IS NULL;
SELECT count(*) FROM t2 GROUP BY GROUPING SETS ((b), length(a)) HAVING length(a) IS NULL;
SELECT abs(-3) AS c0, abs(-3) + 1 AS c1, count(*) FROM t2 GROUP BY GROUPING SETS ((b), abs(-3)) ORDER BY 1, 2, 3;
SELECT b, COALESCE(extract(day from '1999-08-11'::date)::int, 0) AS c1 FROM t2 GROUP BY GROUPING SETS ((b), (extract(day from '1999-08-11'::date)::int)) ORDER BY 1, 2;
SELECT b, (1+1) AS c1 FROM t2 GROUP BY GROUPING SETS ((b), (1+1)) ORDER BY 1, 2;
SELECT b, (1+1) * 3 AS c1 FROM t2 GROUP BY GROUPING SETS ((b), (1+1)) ORDER BY 1, 2;
SELECT b, upper('x') || '' AS c1 FROM t2 GROUP BY GROUPING SETS ((b), upper('x')) ORDER BY 1, 2;
SELECT upper('x') || '' AS c0, upper('x') AS c1 FROM t2 GROUP BY GROUPING SETS ((b), upper('x')) ORDER BY 1, 2;
SELECT b, now() IS NULL AS c1 FROM t2 GROUP BY GROUPING SETS ((b), now()) ORDER BY 1, 2;
SELECT b, random() IS NULL AS c1 FROM t2 GROUP BY GROUPING SETS ((b), random()) ORDER BY 1, 2;
SELECT x.id, (SELECT array_agg(c ORDER BY c) FROM (SELECT x.id + 0 AS c FROM t2 GROUP BY GROUPING SETS ((b), x.id)) s) FROM t2 x ORDER BY 1;
SELECT x.id, (SELECT array_agg(c ORDER BY c) FROM (SELECT x.id AS c FROM t2 GROUP BY GROUPING SETS ((b), x.id)) s) FROM t2 x ORDER BY 1;
SELECT x.id, (SELECT count(*) FROM t2 GROUP BY GROUPING SETS ((b), x.id) HAVING x.id IS NULL) FROM t2 x ORDER BY 1;
