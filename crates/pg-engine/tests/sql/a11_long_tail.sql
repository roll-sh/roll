-- Round 14, agent 11: the small long-tail differences.
-- RETURNING old / new (and the view's own whole row) of a write through a view have the
-- view's row type, not record.
CREATE TABLE a11_t (a int, b text);
INSERT INTO a11_t VALUES (1, 'x');
CREATE VIEW a11_v AS SELECT a, b FROM a11_t;
CREATE VIEW a11_v2 AS SELECT a, b, a + 1 AS c FROM a11_t;
UPDATE a11_v SET a = 2 RETURNING old, new, pg_typeof(old), pg_typeof(new), pg_typeof(a11_v);
INSERT INTO a11_v VALUES (5, 'y') RETURNING old, new, pg_typeof(new), pg_typeof(a11_v);
DELETE FROM a11_v WHERE a = 5 RETURNING old, pg_typeof(old), new;
UPDATE a11_v2 SET a = 7 RETURNING old, new, pg_typeof(new);
MERGE INTO a11_v USING (SELECT 1 AS k) s ON a11_v.a = 7 WHEN MATCHED THEN UPDATE SET b = 'q'
  RETURNING pg_typeof(old), pg_typeof(new), old, new;
-- A DISTINCT by sorting: the Sort returns references to the values its input computes.
CREATE TABLE a11_d (a int, b int, c text);
INSERT INTO a11_d SELECT i % 10, i % 7, 'x' || i FROM generate_series(1, 1000) i;
ANALYZE a11_d;
SET enable_hashagg = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT a + 1 FROM a11_d;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT a + 1, b FROM a11_d LIMIT 3;
-- A GROUP BY key named in ORDER BY takes its direction (transformGroupClause), so do the
-- joins and presorted aggregates below the grouping.
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM a11_d GROUP BY 1 ORDER BY 2 DESC, 1 DESC;
EXPLAIN (COSTS OFF) SELECT a, count(DISTINCT b) FROM a11_d GROUP BY 1 ORDER BY 2 DESC, 1 DESC;
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM a11_d GROUP BY 1, 2 ORDER BY 3, 2 DESC NULLS LAST, 1 DESC;
SET enable_nestloop = off;
SET enable_hashjoin = off;
SET enable_material = off;
EXPLAIN (COSTS OFF)
SELECT x.a, count(DISTINCT x.b), count(*) FROM a11_d x JOIN a11_d y USING (a)
  GROUP BY 1 HAVING count(*) > 1 ORDER BY 2 DESC, 1 DESC LIMIT 3;
SELECT x.a, count(DISTINCT x.b), count(*) FROM a11_d x JOIN a11_d y USING (a)
  GROUP BY 1 HAVING count(*) > 1 ORDER BY 2 DESC, 1 DESC LIMIT 3;
EXPLAIN (COSTS OFF)
SELECT x.a, count(DISTINCT x.b) FROM a11_d x JOIN a11_d y USING (a) GROUP BY 1 ORDER BY 1 DESC;
RESET enable_nestloop;
RESET enable_hashjoin;
RESET enable_material;
RESET enable_hashagg;
-- A grouping shows the GROUP BY keys its select list doesn't name (resjunk); grouping
-- sets' expression keys are computed below them.
CREATE TABLE a11_g (a int, e int);
EXPLAIN (VERBOSE, COSTS OFF) SELECT a * 2, count(*) FROM a11_g t GROUP BY a * 2, e;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a * 2, count(*) FROM a11_g t GROUP BY cube(a * 2, e);
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM a11_g t GROUP BY GROUPING SETS (a, e);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a, a * 2, coalesce(a, 100), e FROM a11_g t GROUP BY GROUPING SETS (a, a * 2, coalesce(a, 100), e);
SET enable_hashagg = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a * 2, count(*) FROM a11_g t GROUP BY rollup(a * 2, e);
RESET enable_hashagg;
CREATE TABLE a11_gv (a int PRIMARY KEY, b int GENERATED ALWAYS AS (a * 2) VIRTUAL,
  c int GENERATED ALWAYS AS (10 + 10) VIRTUAL, d int GENERATED ALWAYS AS (coalesce(a, 100)) VIRTUAL, e int);
INSERT INTO a11_gv VALUES (1), (2);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a11_gv t GROUP BY GROUPING SETS (a, b, c, d, e) HAVING c = 20;
SELECT * FROM a11_gv t GROUP BY GROUPING SETS (a, b, c, d, e) HAVING c = 20 ORDER BY 1, 2, 3, 4, 5;
-- A pulled-up one-row VALUES under grouping sets: its keys computed over its values.
EXPLAIN (COSTS OFF)
SELECT a < b AND b < 3 FROM (VALUES (1, 2)) t(a, b) GROUP BY rollup(a < b AND b < 3) HAVING a < b AND b < 3;
EXPLAIN (COSTS OFF) SELECT NOT a FROM (VALUES (true)) t(a) GROUP BY rollup(NOT a) HAVING NOT NOT a;
-- A CTE's whole row is a Var (`r.*`), its IS [NOT] NULL test not expanded; a CTE proven
-- empty keeps its columns' names.
EXPLAIN (VERBOSE, COSTS OFF)
WITH r(a, b) AS MATERIALIZED (VALUES (1, row(1, 2)), (1, row(NULL, NULL)), (1, NULL), (NULL, row(1, 2)), (NULL, NULL))
SELECT r, r IS NULL AS isnull, r IS NOT NULL AS isnotnull FROM r;
WITH r(a, b) AS MATERIALIZED (VALUES (1, row(1, 2)), (1, row(NULL, NULL)), (1, NULL), (NULL, row(1, 2)), (NULL, NULL))
SELECT r, r IS NULL AS isnull, r IS NOT NULL AS isnotnull FROM r;
EXPLAIN (VERBOSE, COSTS OFF)
WITH r AS MATERIALIZED (SELECT a, e FROM a11_g) SELECT r IS NULL FROM r WHERE r IS NOT NULL;
EXPLAIN (VERBOSE, COSTS OFF)
WITH cte(c) AS MATERIALIZED (SELECT row(1, 2)) SELECT c, (c).f1 FROM cte AS t WHERE false;
-- A FROM subquery proven empty before it was planned: its record's fields are `fN`, its
-- columns qualified only when other relations are in the range table.
CREATE FUNCTION a11_rf(int) RETURNS TABLE (x int, n int) LANGUAGE sql AS 'SELECT $1, 2';
EXPLAIN (VERBOSE, COSTS OFF) SELECT (ss.a).x, (ss.a).n FROM (SELECT a11_rf(1) AS a) ss WHERE false;
EXPLAIN (VERBOSE, COSTS OFF) SELECT ss.a FROM (SELECT generate_series(1, 2) AS a) ss WHERE false;
EXPLAIN (VERBOSE, COSTS OFF) SELECT (ss.a).x, (ss.a).n FROM (SELECT a11_rf(t.a) AS a FROM a11_g t) ss WHERE false;
EXPLAIN (VERBOSE, COSTS OFF) SELECT ss.a, ss.c FROM (SELECT a, e + 1 AS c FROM a11_g OFFSET 0) ss WHERE false;
-- A filter over a FROM-less subquery reading an enclosing query's values gates it
-- (One-Time Filter), its sub-select an InitPlan.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a11_g WHERE (SELECT x FROM (SELECT a AS x) s WHERE (SELECT x > 0)) IS NOT NULL;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a11_g WHERE (SELECT x FROM (SELECT a AS x) s WHERE x > 0) IS NOT NULL;
-- ROWS FROM's calls in one Function Call line; a FROM subquery over a function scan is
-- pulled up (a LATERAL function reads its expressions).
EXPLAIN (VERBOSE, COSTS OFF) SELECT f FROM ROWS FROM (generate_series(1, 3), generate_series(1, 2)) f(x, y);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT jsonb_path_query_array(module -> 'lectures', '$[*]') AS lecture
               FROM unnest(array['{"lectures": [{"id": "1"}]}'::jsonb]) AS um(module)) AS ss,
  jsonb_to_recordset(ss.lecture) AS j (id text);
SELECT * FROM (SELECT jsonb_path_query_array(module -> 'lectures', '$[*]') AS lecture
               FROM unnest(array['{"lectures": [{"id": "1"}]}'::jsonb]) AS um(module)) AS ss,
  jsonb_to_recordset(ss.lecture) AS j (id text);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a + 1 AS c FROM unnest(array[1, 2]) a) ss, generate_series(1, ss.c) g;
-- An INSERT from a FROM subquery not pulled up: its Subquery Scan computes the new row.
CREATE TABLE a11_ins (a int, b int, c int);
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO a11_ins SELECT * FROM (SELECT b, c FROM a11_ins ORDER BY a) ss;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO a11_ins (b, c) SELECT * FROM (SELECT b, c FROM a11_ins ORDER BY a) ss;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO a11_ins SELECT * FROM (SELECT b, c, a FROM a11_ins ORDER BY a) ss;
-- An EXISTS sub-select is planned for its first row (an index-only scan); its InitPlan
-- costs that row.
CREATE TABLE a11_ic (key int4, fruit text);
CREATE UNIQUE INDEX a11_ic_k ON a11_ic (key, fruit);
EXPLAIN (COSTS OFF) INSERT INTO a11_ic VALUES (0, 'Crowberry') ON CONFLICT (key, fruit) DO UPDATE SET fruit = excluded.fruit
  WHERE EXISTS (SELECT 1 FROM a11_ic ii WHERE ii.key = excluded.key);
EXPLAIN SELECT EXISTS (SELECT 1 FROM a11_ic ii WHERE ii.key = 5);
EXPLAIN (COSTS OFF) SELECT EXISTS (SELECT 1 FROM a11_ic ii WHERE ii.key = v.k) FROM (VALUES (1), (2)) v(k);
-- A partitioned table's whole row is its partition's converted; a single partition's
-- columns keep the partitioned table's (inherited) statistics.
CREATE TABLE a11_p (a int, b int) PARTITION BY LIST ((a11_p));
CREATE TABLE a11_p1 PARTITION OF a11_p FOR VALUES IN ('(1,2)');
CREATE TABLE a11_p2 PARTITION OF a11_p FOR VALUES IN ('(2,4)');
EXPLAIN (COSTS OFF) SELECT * FROM a11_p WHERE a11_p = '(1,2)'::a11_p;
CREATE FUNCTION a11_est(text) RETURNS TABLE (estimated int, actual int) LANGUAGE plpgsql AS $$
DECLARE ln text; tmp text[]; first_row bool := true;
BEGIN
  FOR ln IN EXECUTE format('explain analyze %s', $1) LOOP
    IF first_row THEN
      first_row := false;
      tmp := regexp_match(ln, 'rows=(\d*) .* rows=(\d*)');
      RETURN QUERY SELECT tmp[1]::int, tmp[2]::int;
    END IF;
  END LOOP;
END;
$$;
CREATE TABLE a11_sp (i int, a int, b int) PARTITION BY RANGE (i);
CREATE TABLE a11_sp1 PARTITION OF a11_sp FOR VALUES FROM (1) TO (100);
INSERT INTO a11_sp SELECT 1, a / 100, a / 100 FROM generate_series(1, 999) a;
CREATE STATISTICS a11_sps ON (a + 1), a, b FROM a11_sp;
VACUUM ANALYZE a11_sp;
SELECT * FROM a11_est('SELECT a, b FROM a11_sp GROUP BY 1, 2');
SELECT * FROM a11_est('SELECT a + 1, b FROM ONLY a11_sp GROUP BY 1, 2');
-- Row-level security: a cheap leakproof qual of the statement goes ahead of a security
-- barrier view's quals (security level 0), after the policies' cheaper ones.
DROP ROLE IF EXISTS a11_alice;
DROP ROLE IF EXISTS a11_bob;
CREATE ROLE a11_alice;
CREATE ROLE a11_bob;
GRANT CREATE ON SCHEMA public TO a11_alice, a11_bob;
CREATE FUNCTION a11_leak(text) RETURNS bool COST 0.0000001 LANGUAGE plpgsql AS 'BEGIN RETURN true; END';
SET SESSION AUTHORIZATION a11_alice;
CREATE TABLE a11_y1 (a int, b text);
GRANT ALL ON a11_y1 TO a11_bob;
CREATE POLICY p1 ON a11_y1 FOR ALL USING (a % 2 = 0);
CREATE POLICY p2 ON a11_y1 FOR SELECT USING (a > 2);
ALTER TABLE a11_y1 ENABLE ROW LEVEL SECURITY;
CREATE VIEW a11_sbv WITH (security_barrier) AS SELECT * FROM a11_y1 WHERE a11_leak(b);
EXPLAIN (COSTS OFF) SELECT * FROM a11_sbv WHERE (a = 1);
DROP VIEW a11_sbv;
SET SESSION AUTHORIZATION a11_bob;
CREATE VIEW a11_sbv WITH (security_barrier) AS SELECT * FROM a11_y1 WHERE a11_leak(b);
EXPLAIN (COSTS OFF) SELECT * FROM a11_sbv WHERE (a = 1);
EXPLAIN (COSTS OFF) SELECT * FROM a11_sbv WHERE (a < 1);
DROP VIEW a11_sbv;
RESET SESSION AUTHORIZATION;
DROP TABLE a11_y1;
REVOKE CREATE ON SCHEMA public FROM a11_alice, a11_bob;
DROP ROLE a11_alice;
DROP ROLE a11_bob;
-- An EXISTS over an inheritance tree whose members' columns are in another order (a
-- multiply inherited child) is a semi join; the outer side's constant fixes the inner side.
CREATE TABLE a11_i1 (a int, b float, c text);
INSERT INTO a11_i1 SELECT i, i, 'i1' FROM generate_series(1, 10) g(i);
CREATE TABLE a11_i11 (d text) INHERITS (a11_i1);
INSERT INTO a11_i11 SELECT i, i, 'i11', 'i11d' FROM generate_series(1, 10) g(i);
CREATE TABLE a11_i12 (e int[]) INHERITS (a11_i1);
INSERT INTO a11_i12 SELECT i, i, 'i12', '{1,2}'::int[] FROM generate_series(1, 10) g(i);
CREATE TABLE a11_i111 () INHERITS (a11_i11, a11_i12);
INSERT INTO a11_i111 SELECT i, i, 'i111', 'i111d', '{1,1,1}'::int[] FROM generate_series(1, 10) g(i);
ANALYZE a11_i1, a11_i11, a11_i12, a11_i111;
CREATE VIEW a11_iv WITH (security_barrier = true) AS
SELECT *, (SELECT d FROM a11_i11 WHERE a11_i11.a = a11_i1.a LIMIT 1) AS d
FROM a11_i1 WHERE a > 5 AND EXISTS (SELECT 1 FROM a11_i12 WHERE a11_i12.a = a11_i1.a);
UPDATE a11_i1 SET a = a + 1 WHERE a = 8;
EXPLAIN (COSTS OFF) SELECT * FROM a11_iv WHERE b = 8;
SELECT * FROM a11_iv WHERE b = 8;
EXPLAIN (COSTS OFF) SELECT * FROM a11_iv WHERE a = 7;
SELECT * FROM a11_iv WHERE a = 7;
EXPLAIN (COSTS OFF) SELECT * FROM ONLY a11_i1 WHERE a > 5 AND EXISTS (SELECT 1 FROM ONLY a11_i12 WHERE a11_i12.a = a11_i1.a) AND a = 7;
