-- Found by the differential tester (tests/difftest.rs). Postgres folds constants while it
-- plans (eval_const_expressions), after pulling subqueries up and dropping their unused
-- outputs, never for a CTE nothing references, left to right with AND / OR / CASE /
-- COALESCE short-circuiting: a constant that fails to fold fails the statement only if it is
-- still there then. The engine folded while binding and failed every such statement.
CREATE TABLE t (a int, b int, s text, n numeric);
INSERT INTO t VALUES (1, 10, 'x', 1.5), (2, 20, 'y', 'NaN');
CREATE TABLE e (a int);
CREATE TABLE d (a int, b int DEFAULT 1/0, c varchar(2) DEFAULT 'abc');
CREATE FUNCTION f0(x int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT x / 0';
CREATE VIEW v AS SELECT a, 1/0 AS z FROM t;
-- unused outputs of subqueries and views
SELECT 1 FROM (SELECT 1/0 AS c FROM t) s;
SELECT 1 FROM (SELECT 1/0 AS c FROM t OFFSET 0) s;
SELECT 1 FROM (SELECT 1/0 AS c, a FROM t GROUP BY a) s;
SELECT s.a FROM (SELECT a, 1/0 AS c FROM t) s ORDER BY 1;
SELECT a FROM v ORDER BY 1;
SELECT z FROM v;
SELECT 1 FROM (SELECT DISTINCT 1/0 AS c FROM t) s;
SELECT 1 FROM (SELECT 1/0 AS c FROM t ORDER BY 1) s;
SELECT 1 FROM (SELECT 1/0 AS c FROM t LIMIT 1) s;
SELECT 1 FROM (SELECT 'NaN'::numeric::int AS c FROM t) s;
SELECT 1 FROM (SELECT f0(1) AS c FROM t) s;
SELECT 1 FROM (SELECT 1/0 AS c) s WHERE s.c > 0;
SELECT t.a FROM t LEFT JOIN (SELECT a, 1/0 AS z FROM e) s ON s.a = t.a ORDER BY 1;
SELECT 1 FROM (VALUES (1/0)) v(c);
SELECT count(*) FROM (SELECT a, 1/0 AS c FROM t) s;
SELECT count(c) FROM (SELECT a, 1/0 AS c FROM t) s;
-- CTEs
WITH w AS (SELECT 1/0) SELECT 1;
WITH w AS (SELECT 1/0 AS c) SELECT 1 FROM w;
WITH w AS MATERIALIZED (SELECT 1/0 AS c) SELECT 1 FROM w;
WITH w1 AS (SELECT 1/0 AS x), w6 AS (SELECT 1 FROM w1) SELECT 1;
WITH w1 AS MATERIALIZED (SELECT 1/0 AS x), w6 AS (SELECT x FROM w1) SELECT 1;
WITH w1 AS MATERIALIZED (SELECT * FROM e WHERE a <= 1/0), w6 AS (SELECT 1 FROM w1 HAVING false) SELECT 1;
-- short-circuiting
SELECT a FROM t WHERE false AND 1/0 = 1;
SELECT a FROM t WHERE 1/0 = 1 AND false;
SELECT a FROM t WHERE true OR 1/0 = 1 ORDER BY 1;
SELECT a FROM t WHERE 1/0 = 1 OR true;
SELECT false AND 1/0 = 1;
SELECT 1/0 = 1 AND false;
SELECT COALESCE(1, 1/0);
SELECT COALESCE(NULL, 1/0);
SELECT CASE WHEN false THEN 1/0 ELSE 1 END;
SELECT CASE WHEN true THEN 1 ELSE 1/0 END;
SELECT CASE WHEN a > 0 THEN 1/0 ELSE 1 END FROM e;
SELECT CASE 1 WHEN 2 THEN 1/0 ELSE 3 END;
SELECT int4pl(NULL, 1/0);
SELECT a FROM t WHERE f0(1) = 1 AND false;
SELECT 1 FROM t GROUP BY a HAVING false AND 1/0 = 1;
SELECT 1 FROM t GROUP BY a HAVING 1/0 = 1 AND false;
SELECT 1 FROM t JOIN e ON false AND 1/0 = 1;
SELECT 1 FROM t JOIN e ON 1/0 = 1 AND false;
-- evaluated whatever the rows
SELECT 1/0 FROM t WHERE false;
SELECT 1/0 FROM e;
SELECT 1/0 FROM t LIMIT 0;
SELECT a FROM t ORDER BY 1/0;
SELECT 'NaN'::numeric::int FROM e;
SELECT f0(1) FROM e;
SELECT 1 FROM e LIMIT 1/0;
-- EXISTS: its select list is dropped unless it aggregates
SELECT a FROM t WHERE EXISTS (SELECT 1/0 FROM e);
SELECT 1 WHERE EXISTS (SELECT jsonb_array_length('1'));
SELECT EXISTS (SELECT 1/0 FROM e GROUP BY a);
SELECT EXISTS (SELECT 1/0 FROM e HAVING count(*) > 0);
SELECT EXISTS (SELECT 1/0, count(*) FROM e);
-- DML, defaults simplified only where used
INSERT INTO d (a, b, c) VALUES (1, 2, 'ab');
INSERT INTO d (a) VALUES (1);
INSERT INTO d (a) SELECT 1 WHERE false;
INSERT INTO d (a, b) SELECT 1, 2 WHERE false;
UPDATE d SET b = 1/0 WHERE false;
DELETE FROM e WHERE false AND 1/0 = 1;
DELETE FROM e WHERE 1/0 = 1;
-- with two failures, the first one Postgres simplifies
SELECT 'a'::int, 1/0 FROM t WHERE 1/0 = 1;
SELECT 1/0 AS x, 2/0 AS y;
EXPLAIN (COSTS OFF) SELECT 1 FROM (SELECT 1/0 AS c FROM t) s;
EXPLAIN (COSTS OFF) SELECT a FROM t WHERE false AND 1/0 = 1;
EXPLAIN (COSTS OFF) SELECT 1/0 FROM t;
EXPLAIN (COSTS OFF) WITH w1 AS MATERIALIZED (SELECT * FROM e WHERE a <= 1), w6 AS (SELECT 1 FROM w1) SELECT 1;
DROP VIEW v;
DROP FUNCTION f0(int);
