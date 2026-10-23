-- Operators and functions along the search path (OpernameGetCandidates /
-- FuncnameGetCandidates): pg_catalog is searched where the path lists it, first when it
-- doesn't; of the same argument types, the schema searched first wins (the later one is
-- hidden); the best match is then chosen among what's left.
CREATE SCHEMA a11s;
CREATE FUNCTION a11s.ne(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 OPERATOR(pg_catalog.<>) $2';
CREATE OPERATOR a11s.= (LEFTARG = int, RIGHTARG = int, PROCEDURE = a11s.ne);
CREATE OPERATOR a11s.+ (LEFTARG = bigint, RIGHTARG = bigint, PROCEDURE = int8mi);
CREATE FUNCTION a11s.abs(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 42';
CREATE FUNCTION a11s.f(int, bigint) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION a11s.f(bigint, int) RETURNS int LANGUAGE sql AS 'SELECT 2';
SELECT 1 = 2, 1 = 1, abs(-1), abs((-1)::smallint), 5::bigint + 2::bigint, 5 + 2;
SET search_path = a11s, pg_catalog;
SELECT 1 = 2, 1 = 1, abs(-1), abs((-1)::smallint), 5::bigint + 2::bigint, 5 + 2;
SELECT 1 OPERATOR(pg_catalog.=) 2, pg_catalog.abs(-1);
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 = 2;
SELECT f(1, 1);
SET search_path = pg_catalog, a11s;
SELECT 1 = 2, 1 = 1, abs(-1), abs((-1)::smallint), 5::bigint + 2::bigint, 5 + 2;
SET search_path = a11s;
SELECT 1 = 2, 1 = 1, abs(-1), abs((-1)::smallint), 5::bigint + 2::bigint, 5 + 2;
SET search_path = a11s, public;
SELECT 1 = 2, abs(-1);
SET search_path = public, a11s, pg_catalog;
SELECT 1 = 2, abs(-1);
SET search_path = a11s, pg_catalog;
CREATE TABLE a11s.t1 (a int);
CREATE TABLE a11s.t2 (a int);
INSERT INTO a11s.t1 SELECT generate_series(1, 5);
INSERT INTO a11s.t2 SELECT generate_series(3, 7);
ANALYZE a11s.t1, a11s.t2;
SELECT t1.a, t2.a FROM a11s.t1 JOIN a11s.t2 ON t1.a OPERATOR(pg_catalog.=) t2.a ORDER BY 1, 2;
-- (Its hash key is the built-in `=`, shown qualified.)
EXPLAIN (COSTS OFF) SELECT t1.a, t2.a FROM a11s.t1, a11s.t2 WHERE t1.a OPERATOR(pg_catalog.=) t2.a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a OPERATOR(pg_catalog.=) 3 FROM a11s.t1 WHERE a OPERATOR(pg_catalog.=) 2;
CREATE VIEW a11s.v AS SELECT a OPERATOR(pg_catalog.=) 3 AS x FROM a11s.t1;
SELECT pg_get_viewdef('a11s.v');
SELECT count(*) FROM a11s.t1 JOIN a11s.t2 ON t1.a = t2.a;
SELECT a FROM a11s.t1 WHERE a = 3 ORDER BY 1;
RESET search_path;
SELECT pg_get_viewdef('a11s.v');
DROP VIEW a11s.v;
DROP SCHEMA a11s CASCADE;
