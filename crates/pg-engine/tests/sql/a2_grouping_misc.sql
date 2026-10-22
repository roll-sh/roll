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
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3ci GROUP BY x HAVING x IN ('abc', 'def') ORDER BY 1;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT x, count(*) FROM a2_g3ci GROUP BY x;
RESET enable_hashagg;
EXPLAIN (COSTS OFF) SELECT x, count(*) OVER (PARTITION BY x) FROM a2_g3ci;
SELECT x, count(*) OVER (PARTITION BY x) FROM a2_g3ci ORDER BY 1, 2;
CREATE TABLE a2_g3cs (x text COLLATE a2_cs);
INSERT INTO a2_g3cs VALUES ('abc'), ('ABC'), ('def'), ('ghi');
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a2_g3cs GROUP BY x HAVING x = 'abc' COLLATE a2_ci ORDER BY 1;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT x, count(*) FROM a2_g3cs GROUP BY x;
RESET enable_hashagg;
EXPLAIN (COSTS OFF) SELECT x, count(*) OVER (PARTITION BY x) FROM a2_g3cs;
DROP TABLE a2_g3ci, a2_g3cs;

CREATE TABLE a2_tk (unique1 int, unique2 int);
INSERT INTO a2_tk SELECT g, g FROM generate_series(0, 9999) g;
CREATE INDEX ON a2_tk (unique1);
ANALYZE a2_tk;
-- (Casts there and back without functions: the index's column.)
EXPLAIN (COSTS OFF)
SELECT x, y FROM (SELECT unique1::oid AS x, unique2 AS y FROM a2_tk) AS t
WHERE x::integer = 1 GROUP BY GROUPING SETS (x, y) ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT * FROM a2_tk WHERE unique1::oid = 5::oid;
DROP TABLE a2_tk;

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
-- (A constant grouping-set key is computed below the aggregation: a reference over a Sort.)
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 1 AS x, q1, sum(q2) FROM a2_i8 i1 GROUP BY GROUPING SETS (1, 2)) ss
WHERE x = 1 AND q1 = 123;
SET enable_hashagg = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 1 AS x, q1, sum(q2) FROM a2_i8 i1 GROUP BY GROUPING SETS (1, 2)) ss
WHERE x = 1 AND q1 = 123;
SELECT * FROM (SELECT 1 AS x, q1, sum(q2) FROM a2_i8 i1 GROUP BY GROUPING SETS (1, 2)) ss
ORDER BY 1, 2;
RESET enable_hashagg;
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

-- (A Values Scan under a ProjectSet returns exactly the columns it reads.)
EXPLAIN (VERBOSE, COSTS OFF)
SELECT generate_series(1, a) AS g FROM (VALUES (1, 1), (2, 2)) AS t (a, b) GROUP BY ROLLUP (g) ORDER BY 1;
