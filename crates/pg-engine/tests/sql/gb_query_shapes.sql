-- Grouping sets, SRFs in GROUP BY, WITH validation, sampling and other query-level rules
CREATE TABLE gb_g (a int PRIMARY KEY, b int, c int, d int);
INSERT INTO gb_g VALUES (1, 1, 1, 10), (2, 2, 1, 20), (3, 2, 2, 30);
SELECT a, d, grouping(a, b, c) FROM gb_g GROUP BY GROUPING SETS ((a, b), (a, c)) ORDER BY 3, 1;
SET enable_hashagg = off;
SELECT b, c, sum(d) FROM gb_g GROUP BY ROLLUP (b, c);
RESET enable_hashagg;
SELECT (SELECT grouping(g.b)) FROM gb_g g GROUP BY g.b ORDER BY 1;
SELECT (SELECT grouping(a, b) FROM gb_g) FROM gb_g GROUP BY a, b;
CREATE TABLE gb_u (h bit(4), s xid);
SELECT count(*) FROM gb_u GROUP BY ROLLUP (h, s);
SELECT array_agg(h ORDER BY h) FROM gb_u GROUP BY GROUPING SETS ((h, s), (h));
SELECT b, unnest('{1,1,3}'::int[]) AS u, count(*) FROM gb_g GROUP BY b, unnest('{1,1,3}'::int[]) ORDER BY 1, 2;
SELECT b, generate_series(1, 2) AS g, count(*) FROM gb_g GROUP BY 1, 2 ORDER BY 1, 2;
SELECT * FROM generate_series(1, generate_series(1, 3));
SELECT ( VALUES ((SELECT 1)) );
SELECT (SELECT 1 UNION SELECT 2 ORDER BY 1 LIMIT 1);
SELECT (WITH c(foo) AS (VALUES (a)) SELECT (SELECT foo FROM c)) FROM gb_g ORDER BY 1;
WITH RECURSIVE t(j) AS (
  WITH RECURSIVE s(i) AS (VALUES (1) UNION ALL SELECT i + 1 FROM s WHERE i < 3)
  SELECT i FROM s
  UNION ALL
  SELECT j + 1 FROM t WHERE j < 3)
SELECT j, count(*) FROM t GROUP BY j ORDER BY j;
WITH RECURSIVE x(n) AS (
  WITH x1 AS (SELECT 1 FROM x) SELECT 0 UNION SELECT * FROM x1)
SELECT * FROM x;
WITH RECURSIVE x(n) AS (DELETE FROM gb_g RETURNING (SELECT n FROM x)) SELECT * FROM x;
SELECT * FROM (WITH t AS (UPDATE gb_g SET b = b RETURNING *) SELECT * FROM t) ss;
WITH a(x) AS (SELECT 1 UNION (WITH i AS (SELECT 2) SELECT * FROM a UNION SELECT * FROM i)) SELECT * FROM a;
WITH RECURSIVE foo(i) AS (SELECT i FROM (VALUES (1)) t(i) UNION ALL SELECT (i + 1)::numeric(10,0) FROM foo WHERE i < 3)
SELECT * FROM foo;
WITH RECURSIVE foo(i) AS (SELECT i::numeric(3,0) FROM (VALUES (1)) t(i) UNION ALL SELECT (i + 1)::numeric(10,0) FROM foo WHERE i < 3)
SELECT * FROM foo;
WITH RECURSIVE g(f, t) AS (SELECT 1, 2 UNION ALL SELECT 2, 3 UNION ALL SELECT g.t, g.t + 1 FROM g WHERE g.t < 4)
  SEARCH DEPTH FIRST BY f, t SET seq
SELECT * FROM g;
WITH RECURSIVE g(f, t) AS (SELECT 1, 2 UNION ALL SELECT g.t, g.t + 1 FROM g WHERE g.t < 4)
  CYCLE f, t SET is_cycle TO true DEFAULT 55 USING path
SELECT * FROM g;
WITH RECURSIVE g(f, t) AS (SELECT 1, 2 UNION ALL SELECT g.t, g.t + 1 FROM g WHERE g.t < 4)
  CYCLE f, t SET is_cycle TO point '(1,1)' DEFAULT point '(0,0)' USING path
SELECT * FROM g;
SELECT a FROM gb_g ORDER BY a FETCH FIRST NULL ROWS WITH TIES;
SELECT DISTINCT ON (b, c) b, a, c FROM gb_g ORDER BY b, a, c;
SELECT b FROM gb_g GROUP BY b FOR UPDATE;
SELECT DISTINCT b FROM gb_g FOR SHARE;
SELECT count(*) FROM gb_g FOR NO KEY UPDATE;
CREATE TABLE gb_ts (id int, name text) WITH (fillfactor = 10);
INSERT INTO gb_ts SELECT i, repeat(i::text, 200) FROM generate_series(0, 9) s(i);
SELECT id FROM gb_ts TABLESAMPLE SYSTEM (50) REPEATABLE (0);
SELECT id FROM gb_ts TABLESAMPLE BERNOULLI (50) REPEATABLE (0);
SELECT id FROM gb_ts TABLESAMPLE BERNOULLI (5.5) REPEATABLE (1);
SELECT random(min => 7, max => 7), random(min => 2::bigint, max => 2), random(min => 1.5, max => 1.5);
PREPARE gb_p AS SELECT * FROM gb_g WHERE a = 1;
ALTER TABLE gb_g ADD COLUMN e int;
EXECUTE gb_p;
DEALLOCATE gb_p;
CREATE TEMP TABLE gb_oc (f1 int PRIMARY KEY, f2 text);
INSERT INTO gb_oc VALUES (1, 'a');
INSERT INTO gb_oc VALUES (1, 'b'), (2, 'c') ON CONFLICT (f1) DO UPDATE SET f2 = excluded.f2
  RETURNING old.tableoid::regclass, old.ctid, old.f2, new.ctid, new.f2;
BEGIN;
DECLARE gb_bc BINARY CURSOR FOR SELECT 1;
SELECT name, is_binary FROM pg_cursors WHERE name = 'gb_bc';
COMMIT;
DROP TABLE gb_oc;
DROP TABLE gb_ts;
DROP TABLE gb_u;
DROP TABLE gb_g;
