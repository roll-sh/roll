-- Set-returning SQL functions inlined in FROM (inline_set_returning_function): their
-- arguments substituted into the body, the body a subquery under the function's name.
CREATE TABLE gpl_i8 (q1 int8, q2 int8);
INSERT INTO gpl_i8 VALUES (1, 2), (3, 4), (5, 6);
CREATE FUNCTION gpl_win(group_size bigint) RETURNS SETOF integer[] AS $$
    SELECT array_agg(s) OVER w
      FROM generate_series(1, 5) s
    WINDOW w AS (ORDER BY s ROWS BETWEEN CURRENT ROW AND group_size FOLLOWING)
$$ LANGUAGE sql STABLE;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_win(2);
SELECT * FROM gpl_win(2);
CREATE FUNCTION gpl_series(int) RETURNS SETOF int AS $$
  SELECT g FROM generate_series(1, $1) g WHERE g > (SELECT $1 - 3)
$$ LANGUAGE sql STABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_series(5);
SELECT * FROM gpl_series(5);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_i8, gpl_series(gpl_i8.q1::int) x;
SELECT * FROM gpl_i8, gpl_series(gpl_i8.q1::int) x ORDER BY 1, 3;
-- A record result with column definitions.
CREATE FUNCTION gpl_ats(anyarray) RETURNS SETOF record AS $$
  SELECT i AS "index", $1[i] AS "value" FROM generate_subscripts($1, 1) i
$$ LANGUAGE sql IMMUTABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_ats(array['one', 'two']) AS t(f1 numeric(4,2), f2 text);
SELECT * FROM gpl_ats(array['one', 'two']) AS t(f1 numeric(4,2), f2 text);
SELECT * FROM gpl_ats(array['one', 'two']) AS t(f1 point, f2 text);
-- Coercions to the result type: constants folded; a set operation's columns
-- coerced over it as a subquery.
CREATE TYPE gpl_rt AS (f1 numeric(35,6), f2 numeric(35,2));
CREATE FUNCTION gpl_consts() RETURNS SETOF gpl_rt AS $$ SELECT 7.136178319899999964, 7.136178319899999964; $$ LANGUAGE sql IMMUTABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_consts();
SELECT * FROM gpl_consts();
CREATE FUNCTION gpl_union() RETURNS SETOF gpl_rt AS $$ SELECT 1, 2 UNION SELECT 3, 4 ORDER BY 1; $$ LANGUAGE sql IMMUTABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_union();
SELECT * FROM gpl_union();
CREATE FUNCTION gpl_unionall() RETURNS SETOF text AS $$ SELECT 'foo'::varchar UNION ALL SELECT 'bar'::varchar; $$ LANGUAGE sql STABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_unionall();
SELECT * FROM gpl_unionall();
-- A whole-row argument from the left.
CREATE FUNCTION gpl_q2(t gpl_i8) RETURNS int8 AS $$ SELECT t.q2 $$ LANGUAGE sql IMMUTABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM gpl_i8, gpl_q2(gpl_i8) f(x);
SELECT x FROM gpl_i8, gpl_q2(gpl_i8) f(x) ORDER BY 1;
CREATE FUNCTION gpl_q2b(t gpl_i8) RETURNS TABLE(ret1 int8) AS $$ SELECT gpl_q2(t) OFFSET 0 $$ LANGUAGE sql IMMUTABLE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM gpl_i8, gpl_q2b(gpl_i8) f(x);
SELECT x FROM gpl_i8, gpl_q2b(gpl_i8) f(x) ORDER BY 1;
-- Set operations read other than as they are: a Result over an append relation, a
-- Subquery Scan over a set operation's subquery.
EXPLAIN (VERBOSE, COSTS OFF) SELECT x::text FROM (SELECT 'foo'::varchar x UNION ALL SELECT 'bar'::varchar) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x::text FROM (SELECT 'foo'::varchar x UNION SELECT 'bar'::varchar) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1, 2 UNION SELECT 3, 4 ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT x FROM (SELECT 1 x, 2 y UNION SELECT 3, 4) s;
SELECT x::text FROM (SELECT 'foo'::varchar x UNION SELECT 'bar'::varchar) s ORDER BY 1;
