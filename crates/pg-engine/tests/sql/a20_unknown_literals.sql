-- Round 16 (agent 20): untyped literals in a SELECT's output are text (PostgreSQL 10+,
-- resolveTargetListUnknowns), resolved in the target list itself: EXPLAIN VERBOSE shows
-- `'lit'::text`. Set-operation leaves and an INSERT's SELECT keep them unknown for the set
-- operation / the target columns to resolve; a GROUP BY / ORDER BY / DISTINCT key that is
-- one is text in every case (addTargetToGroupList, addTargetToSortList).
CREATE TABLE a20_t (i int, d date);
CREATE TABLE a20_s (k int, v text);
INSERT INTO a20_s VALUES (1, 'one'), (2, 'two');
SELECT 'lit';
SELECT NULL;
SELECT 'a' x, 1 y, NULL z;
SELECT pg_typeof('a'), pg_typeof(x) FROM (SELECT 'a' x) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 'lit';
EXPLAIN (VERBOSE, COSTS OFF) SELECT NULL, 'a' x, 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 'a' COLLATE "C";
SELECT 'a', k FROM a20_s ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT 'a', k FROM a20_s;
SELECT 'a' x, count(*) FROM a20_s GROUP BY 1;
SELECT pg_typeof(x) FROM (SELECT 'a' x FROM a20_s GROUP BY 1) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 'a', generate_series(1, 2);
EXPLAIN (VERBOSE, COSTS OFF) SELECT (SELECT 'x');
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT 'x' b OFFSET 0) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT b || 'y' FROM (SELECT 'x' b OFFSET 0) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT s.k, q.c FROM a20_s s, (SELECT 'c' c OFFSET 0) q;
EXPLAIN (VERBOSE, COSTS OFF)
WITH c AS MATERIALIZED (SELECT 1 a, 'x' b) SELECT * FROM c;
WITH c AS (SELECT 1 a, 'x' b) SELECT pg_typeof(b) FROM c;
-- Set operations resolve their leaves' literals together.
EXPLAIN (VERBOSE, COSTS OFF) SELECT 'a' UNION SELECT 'b';
EXPLAIN (VERBOSE, COSTS OFF) SELECT 'a', 1 UNION ALL SELECT 'b', 2;
SELECT '1' UNION ALL SELECT 2;
SELECT x, pg_typeof(x) FROM (SELECT 1 UNION SELECT '2') s(x) ORDER BY 1;
SELECT 'a' UNION SELECT 'a' ORDER BY 1;
-- VALUES: select_common_type, all unknown is text.
VALUES ('a'), ('b');
SELECT pg_typeof(x) FROM (VALUES ('a')) v(x);
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM (VALUES ('a'), ('b')) v(x);
-- INSERT ... SELECT: the literals take the target columns' types.
INSERT INTO a20_t SELECT '1', '2020-01-01';
INSERT INTO a20_t SELECT '2', '2020-01-02' UNION ALL SELECT '3', NULL;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO a20_t SELECT '1', '2020-01-01';
-- ... but not a subquery's or an ORDER BY key's, which are text.
INSERT INTO a20_t SELECT * FROM (SELECT '4', '2020-01-04') s;
INSERT INTO a20_t SELECT '5' ORDER BY 1;
INSERT INTO a20_t SELECT DISTINCT '5';
WITH c AS (SELECT '6' x) INSERT INTO a20_t SELECT x FROM c;
SELECT * FROM a20_t ORDER BY i;
-- RETURNING
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO a20_t VALUES (7, NULL) RETURNING 'r', i;
-- CREATE TABLE AS, views
CREATE TABLE a20_ctas AS SELECT 'x' a, NULL b, 1 c;
SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute
WHERE attrelid = 'a20_ctas'::regclass AND attnum > 0 ORDER BY attnum;
CREATE VIEW a20_v AS SELECT 'x' a, NULL b, k FROM a20_s;
SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute
WHERE attrelid = 'a20_v'::regclass AND attnum > 0 ORDER BY attnum;
SELECT pg_get_viewdef('a20_v'::regclass);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a20_v;
CREATE VIEW a20_vu AS SELECT 'x' a UNION SELECT 'y';
SELECT pg_get_viewdef('a20_vu'::regclass);
-- A prepared statement's untyped parameter in the output is text.
PREPARE a20_p AS SELECT $1;
SELECT parameter_types, result_types FROM pg_prepared_statements WHERE name = 'a20_p';
EXECUTE a20_p('z');
DEALLOCATE a20_p;
-- A SQL function's result: text, coerced to the declared type only if assignable.
CREATE FUNCTION a20_f() RETURNS varchar LANGUAGE sql AS $$ SELECT 'f' $$;
SELECT a20_f(), pg_typeof(a20_f());
CREATE FUNCTION a20_g() RETURNS int LANGUAGE sql AS $$ SELECT '1' $$;
DROP VIEW a20_vu;
DROP VIEW a20_v;
DROP FUNCTION a20_f();
DROP TABLE a20_ctas, a20_t, a20_s;
