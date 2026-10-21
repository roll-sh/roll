-- HAVING clauses moved to WHERE under nondeterministic collations (Postgres 18.6's
-- expression_has_grouping_conflict: a key compared directly under its own collation moves,
-- any other use of it stays); a key an equality under its own collation fixes groups
-- nothing; hashed grouping sets leave keys grouped only for being functionally dependent
-- out of the hash; unread subquery outputs stay in its target list as NULLs, constants
-- shown as constants above; a user-defined fixed-length type's width; eqsel with the
-- variable on the right without a commutator.
CREATE COLLATION a2_ci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE COLLATION a2_cs (provider = icu, locale = '', deterministic = true);
CREATE TABLE a2_g3ci (x text COLLATE a2_ci);
INSERT INTO a2_g3ci VALUES ('abc'), ('ABC'), ('def'), ('ghi');
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_cs;
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_cs;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_ci;
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_ci;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING upper(x) = 'ABC' COLLATE a2_ci;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_cs AND x >= 'a' COLLATE a2_ci;
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_cs AND x >= 'a' COLLATE a2_ci;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x = 'abc' COLLATE a2_ci AND x >= 'a' COLLATE a2_ci;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x <> 'abc' COLLATE a2_ci ORDER BY 1;
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x IN ('abc', 'def') ORDER BY 1;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING (CASE x WHEN 'abc' COLLATE a2_cs THEN true ELSE false END);
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING (CASE x WHEN 'abc' COLLATE a2_ci THEN true ELSE false END);
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING ROW(x, 1) < ROW('ABC' COLLATE a2_cs, 1) ORDER BY 1;
DROP TABLE a2_g3ci;

CREATE TEMP TABLE a2_gs3 (a integer, b integer, c integer, d integer);
INSERT INTO a2_gs3 VALUES (1, 1, 1, 1), (2, 2, 2, 2);
ALTER TABLE a2_gs3 ADD PRIMARY KEY (a);
SELECT a, d, grouping(a, b, c) FROM a2_gs3 GROUP BY GROUPING SETS ((a, b), (a, c));
SELECT a, d, count(*) FROM a2_gs3 GROUP BY GROUPING SETS ((a, b));
SET enable_sort = off;
SELECT a, d, count(*) FROM a2_gs3 GROUP BY a, b;
RESET enable_sort;

CREATE TEMP TABLE a2_i8 (q1 int8, q2 int8);
INSERT INTO a2_i8 VALUES (1, 2), (3, 4);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT q1 FROM (
  WITH t_cte AS (SELECT * FROM a2_i8 t)
  SELECT q1, (SELECT q2 FROM t_cte WHERE t_cte.q1 = i8.q1) AS t_sub FROM a2_i8 i8
) ss;
SELECT q1 FROM (SELECT q1, 1 / (q2 - q2) AS z FROM a2_i8 i8 ORDER BY q1) ss;

CREATE TYPE a2_int8alias;
CREATE FUNCTION a2_int8aliasin(cstring) RETURNS a2_int8alias
  STRICT IMMUTABLE LANGUAGE internal AS 'int8in';
CREATE FUNCTION a2_int8aliasout(a2_int8alias) RETURNS cstring
  STRICT IMMUTABLE LANGUAGE internal AS 'int8out';
CREATE TYPE a2_int8alias (input = a2_int8aliasin, output = a2_int8aliasout, like = int8);
CREATE CAST (int8 AS a2_int8alias) WITHOUT FUNCTION;
CREATE FUNCTION a2_int8aliaseq(int8, a2_int8alias) RETURNS bool
  STRICT IMMUTABLE LANGUAGE internal AS 'int8eq';
CREATE OPERATOR = (procedure = a2_int8aliaseq, leftarg = int8, rightarg = a2_int8alias,
                   restrict = eqsel, join = eqjoinsel);
CREATE TABLE a2_ec1 (ff int8 PRIMARY KEY, f1 a2_int8alias);
CREATE TABLE a2_ec2 (xf int8 PRIMARY KEY, x1 a2_int8alias);
EXPLAIN SELECT * FROM a2_ec2 WHERE '42'::int8 = x1;
EXPLAIN SELECT * FROM a2_ec1, a2_ec2 WHERE ff = x1 AND '42'::int8 = x1;
DROP TABLE a2_ec1, a2_ec2;
DROP TYPE a2_int8alias CASCADE;
DROP COLLATION a2_ci;
DROP COLLATION a2_cs;
