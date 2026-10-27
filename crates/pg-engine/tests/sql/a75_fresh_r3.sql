-- Round 3 difftest finds (agent 75).
-- Self-join elimination over a unique composite column: the kept scan's null test is of the
-- value, not its fields (a row with a NULL field stays).
CREATE TYPE a75r3c AS (a text COLLATE "C", b int);
CREATE TABLE a75r3t (id int PRIMARY KEY, c a75r3c, n int);
INSERT INTO a75r3t VALUES (1, ROW('a%', -1), 3), (2, NULL, 3), (3, ROW(NULL, 1), 3), (4, ROW('foo', NULL), 1), (5, ROW('ß', 2), 4);
CREATE UNIQUE INDEX a75r3t_u ON a75r3t (c);
SELECT x.id, y.id FROM a75r3t x JOIN a75r3t y ON x.c = y.c ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT x.id, y.id FROM a75r3t x JOIN a75r3t y ON x.c = y.c ORDER BY 1, 2;
-- A DISTINCT aggregate over a nondeterministic key: the input sorted under its collation,
-- so max / min keep the newer of two values it finds equal.
CREATE COLLATION a75r3ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TYPE a75r3n AS (a text COLLATE a75r3ci, b int);
CREATE TABLE a75r3d (id int PRIMARY KEY, c a75r3n, s text COLLATE a75r3ci);
INSERT INTO a75r3d VALUES (1, ROW('a', 1), 'a'), (2, ROW('A', 1), 'A');
SELECT max(c), count(DISTINCT (c).a) FROM a75r3d;
SELECT max(s), count(DISTINCT s), min(s) FROM a75r3d;
SELECT array_agg(DISTINCT c) FROM a75r3d;
-- An aggregate naming only an outer query's columns belongs to it, also when folding drops
-- them.
CREATE TABLE a75r3e (id int);
CREATE VIEW a75r3v AS SELECT max(1::text) AS c0, 1 AS c1 FROM a75r3e;
SELECT (SELECT count(1) FILTER (WHERE starts_with(NULL, t1.c0)) FROM a75r3e t3) AS c1 FROM a75r3v t1;
SELECT (SELECT count(1) FILTER (WHERE starts_with(NULL, t1.c0)) FROM a75r3t t3 WHERE false) AS c1 FROM a75r3v t1;
-- A CASE whose ELSE is a scalar sub-select is named after the sub-select's column.
SELECT CASE WHEN false THEN 1 ELSE (SELECT 1) END;
SELECT CASE WHEN false THEN 1 ELSE (SELECT 1 AS x) END;
SELECT CASE WHEN true THEN (SELECT 1 AS y) END;
SELECT (SELECT 1)::int, -(SELECT 1 AS q), (SELECT 1 AS q)::text;
-- A SQL function neither inlined nor run while planning: its body's failing constant fails
-- only when it runs.
CREATE FUNCTION a75r3f(p int) RETURNS int LANGUAGE sql STABLE STRICT AS $$ SELECT CASE WHEN p > 100 THEN 9 ELSE ('{"a": 2.50}'::jsonb ->> 'a')::int END $$;
SELECT (SELECT a75r3f(1) FROM a75r3e WHERE NULL) FROM a75r3t ORDER BY 1 LIMIT 1;
SELECT a75r3f(1);
-- A pulled-up view's failing output read in a join clause inside an EXISTS (an InitPlan):
-- simplified, and failing, though the view is empty.
CREATE TABLE a75r3x (a int, b text);
CREATE VIEW a75r3w AS SELECT s2.c3 AS c1 FROM (a75r3x t1 RIGHT JOIN (SELECT ('"s"'::jsonb - 'a') AS c3 FROM a75r3e) s2 ON true) WHERE NULL;
SELECT 1 FROM a75r3t t3 WHERE EXISTS (SELECT t4.a FROM a75r3x t4 LEFT JOIN (SELECT t6.c1 ->> 'b' AS c0 FROM a75r3w t6) s5 ON t4.b = s5.c0);
DROP VIEW a75r3w, a75r3v;
DROP FUNCTION a75r3f;
DROP TABLE a75r3t, a75r3d, a75r3e, a75r3x;
DROP TYPE a75r3c, a75r3n;
DROP COLLATION a75r3ci;
