-- Round 3 of postgres_fdw (agent 110): the engine-side pieces a loopback server shows.
-- A missing operator names types the search path doesn't see qualified (format_type_be).
CREATE SCHEMA a110s;
CREATE TYPE a110s.mood AS ENUM ('foo', 'bar');
CREATE TABLE a110_t (id int PRIMARY KEY, m a110s.mood, c6 varchar(10), c3 text, n int);
INSERT INTO a110_t VALUES (1, 'foo', 'foo', 'a', 10), (2, 'bar', 'x', 'b', 20), (3, 'foo', 'y', 'c', 30);
SELECT * FROM a110_t WHERE m LIKE 'foo';
SELECT * FROM a110_t WHERE m = 'foo'::text;
SET search_path = a110s, public;
SELECT * FROM a110_t WHERE m LIKE 'foo';
RESET search_path;
-- A VARIADIC aggregate's call shows its arguments' array as VARIADIC (aggvariadic).
CREATE FUNCTION a110_least_accum(anyelement, variadic anyarray) RETURNS anyelement
  LANGUAGE sql AS 'SELECT least($1, min($2[i])) FROM generate_subscripts($2, 1) g(i)';
CREATE AGGREGATE a110_least_agg(variadic items anyarray) (stype = anyelement, sfunc = a110_least_accum);
CREATE VIEW a110_v AS SELECT a110_least_agg(n) AS a, a110_least_agg(VARIADIC ARRAY[n, id]) AS b FROM a110_t;
SELECT pg_get_viewdef('a110_v');
SELECT * FROM a110_v;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a110_least_agg(n, id) FROM a110_t;
-- A simple CASE whose operand is coerced to the comparison's type.
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM a110_t WHERE CASE c6 WHEN 'foo' THEN true ELSE c3 < 'bar' END;
CREATE VIEW a110_w AS SELECT CASE c6 WHEN 'foo' THEN 1 WHEN 'x' THEN 2 END AS k FROM a110_t;
SELECT pg_get_viewdef('a110_w');
-- A SELECT's data-modifying CTE is planned (its scan by the primary key).
EXPLAIN (COSTS OFF) WITH u AS (UPDATE a110_t SET n = n + 1 WHERE id = 2 RETURNING *) SELECT * FROM u;
WITH u AS (UPDATE a110_t SET n = n + 1 WHERE id = 2 RETURNING *) SELECT * FROM u;
EXPLAIN (COSTS OFF) WITH d AS (DELETE FROM a110_t WHERE id = 3 RETURNING id) SELECT count(*) FROM d;
WITH d AS (DELETE FROM a110_t WHERE id = 3 RETURNING id) SELECT count(*) FROM d;
SELECT * FROM a110_t ORDER BY id;
DROP VIEW a110_v, a110_w;
DROP AGGREGATE a110_least_agg(anyarray);
DROP FUNCTION a110_least_accum(anyelement, anyarray);
DROP TABLE a110_t;
DROP SCHEMA a110s CASCADE;
