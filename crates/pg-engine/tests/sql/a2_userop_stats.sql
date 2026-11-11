-- Estimates for clauses of user-defined operators (RESTRICT = scalarltsel / eqsel): the
-- operator's function tested on the statistics' values (a histogram not sorted by it: the
-- fraction it accepts), only for a leakproof function unless the user the scan checks as
-- (a view's owner) may read the column; a view read twice is named in range-table order;
-- a pulled-up subquery's scan gets the join's parameterized paths; a subquery scan's
-- filter estimated over its outputs.
DROP ROLE IF EXISTS a2_uo_owner;
DROP ROLE IF EXISTS a2_uo_reader;
CREATE ROLE a2_uo_owner;
CREATE ROLE a2_uo_reader;
GRANT CREATE ON SCHEMA public TO a2_uo_owner, a2_uo_reader;
CREATE FUNCTION a2_uo_lt(integer, integer) RETURNS boolean
  AS 'int4lt' LANGUAGE internal IMMUTABLE STRICT;
CREATE FUNCTION a2_uo_eq(integer, integer) RETURNS boolean
  AS 'int4eq' LANGUAGE internal IMMUTABLE STRICT;
SET SESSION AUTHORIZATION a2_uo_owner;
CREATE TABLE a2_uo AS SELECT x AS a, 10001 - x AS b, x % 10 AS c FROM generate_series(1, 10000) x;
CREATE INDEX ON a2_uo (a);
CREATE INDEX ON a2_uo (abs(a));
SET default_statistics_target = 10000;
ANALYZE a2_uo;
RESET default_statistics_target;
CREATE OPERATOR <<< (procedure = a2_uo_lt, leftarg = integer, rightarg = integer,
                     restrict = scalarltsel);
CREATE OPERATOR === (procedure = a2_uo_eq, leftarg = integer, rightarg = integer,
                     restrict = eqsel, commutator = ===);
CREATE VIEW a2_uo_v AS SELECT * FROM a2_uo WHERE b <<< 5;
CREATE VIEW a2_uo_sbv WITH (security_barrier = true) AS SELECT * FROM a2_uo WHERE b <<< 5;
EXPLAIN SELECT * FROM a2_uo WHERE b <<< 5;
EXPLAIN SELECT * FROM a2_uo WHERE 5 <<< b;
EXPLAIN SELECT * FROM a2_uo WHERE c === 3;
EXPLAIN SELECT * FROM a2_uo WHERE 3 === c;
EXPLAIN SELECT * FROM a2_uo WHERE a === 3;
EXPLAIN SELECT * FROM a2_uo WHERE abs(a) <<< 5;
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo_v x, a2_uo_v y WHERE x.a = y.b;
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo x, a2_uo y WHERE x.a = y.b AND abs(y.a) <<< 5;
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo_sbv x, a2_uo_sbv y WHERE x.a = y.b;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM a2_uo WHERE b < 5) x,
  (SELECT * FROM a2_uo WHERE b < 5) y WHERE x.a = y.b;
SELECT * FROM a2_uo_v x, a2_uo_v y WHERE x.a = y.b;
-- (The query's own relation is named before the view's.)
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo_v, a2_uo WHERE a2_uo_v.a = a2_uo.b AND a2_uo.c = 1;
EXPLAIN (COSTS OFF) WITH w AS (SELECT * FROM a2_uo WHERE c = 1)
  SELECT * FROM a2_uo WHERE a IN (SELECT b FROM w);
GRANT SELECT ON a2_uo_v TO PUBLIC;
GRANT SELECT ON a2_uo_sbv TO PUBLIC;
SET SESSION AUTHORIZATION a2_uo_reader;
-- (As the views' owner: their statistics.)
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo_v x, a2_uo_v y WHERE x.a = y.b;
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo_v x, a2_uo_v y WHERE x.a = y.b AND abs(y.a) <<< 5;
EXPLAIN (COSTS OFF) SELECT * FROM a2_uo_sbv x, a2_uo_sbv y WHERE x.a = y.b AND abs(y.a) <<< 5;
EXPLAIN SELECT * FROM a2_uo_sbv y WHERE abs(y.a) <<< 5;
SET SESSION AUTHORIZATION a2_uo_owner;
GRANT SELECT (a, b) ON a2_uo TO PUBLIC;
SET SESSION AUTHORIZATION a2_uo_reader;
-- (Column privileges: the columns' statistics, not the expression index's.)
EXPLAIN SELECT a, b FROM a2_uo WHERE b <<< 5;
EXPLAIN SELECT a, b FROM a2_uo WHERE abs(a) <<< 5;
EXPLAIN (COSTS OFF) SELECT x.a, y.b FROM a2_uo x, a2_uo y WHERE x.a = y.b AND abs(y.a) <<< 5;
RESET SESSION AUTHORIZATION;
-- Row-level security on the table: its statistics only for leakproof functions.
ALTER TABLE a2_uo ENABLE ROW LEVEL SECURITY;
CREATE POLICY a2_uo_p ON a2_uo USING (true);
SET SESSION AUTHORIZATION a2_uo_reader;
EXPLAIN SELECT a, b FROM a2_uo WHERE b <<< 5;
EXPLAIN SELECT a, b FROM a2_uo WHERE b < 5;
RESET SESSION AUTHORIZATION;
-- Relations of set operation arms are named in plan order.
CREATE TABLE a2_uo_t (unique1 int, fivethous int);
INSERT INTO a2_uo_t SELECT g, g % 5000 FROM generate_series(1, 10000) g;
ANALYZE a2_uo_t;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM (SELECT unique1 FROM a2_uo_t INTERSECT SELECT fivethous FROM a2_uo_t) ss;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM (SELECT fivethous FROM a2_uo_t INTERSECT SELECT unique1 FROM a2_uo_t) ss;
DROP TABLE a2_uo_t;
DROP OWNED BY a2_uo_owner;
DROP OWNED BY a2_uo_reader;
DROP FUNCTION a2_uo_lt(integer, integer);
DROP FUNCTION a2_uo_eq(integer, integer);
DROP ROLE a2_uo_owner;
DROP ROLE a2_uo_reader;
