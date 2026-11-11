-- SQL-standard function bodies: pg_get_functiondef / pg_get_function_sqlbody print the
-- stored query trees
CREATE SCHEMA sb;
SET search_path = sb, public;
CREATE TABLE t (a int PRIMARY KEY, b text DEFAULT 'z', c int);
CREATE TABLE u (a int, d int);
CREATE FUNCTION r1(x int) RETURNS int LANGUAGE sql IMMUTABLE RETURN x + 1;
CREATE FUNCTION r2(x int, y int) RETURNS int LANGUAGE sql RETURN (SELECT count(*) FROM t WHERE a > x AND c < y)::int;
CREATE FUNCTION r3(int) RETURNS bool LANGUAGE sql STRICT RETURN $1 > 0 OR $1 IS NULL;
CREATE FUNCTION r4(x text) RETURNS text LANGUAGE sql RETURN upper(x) || 'a';
CREATE FUNCTION r5(x int) RETURNS int LANGUAGE sql RETURN (SELECT x);
CREATE FUNCTION r6(x int) RETURNS int LANGUAGE sql RETURN CASE WHEN x > 0 THEN (SELECT max(a) FROM t) ELSE 0 END;
CREATE FUNCTION b1(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC INSERT INTO t VALUES (x, 'a'); SELECT count(*) FROM t WHERE a > x; END;
CREATE FUNCTION b2(x int) RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT x + 1; END;
CREATE FUNCTION b3(x int) RETURNS SETOF t LANGUAGE sql BEGIN ATOMIC SELECT * FROM t WHERE t.a = x ORDER BY b; END;
CREATE FUNCTION b4(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC
  UPDATE t SET b = 'q', c = c + x WHERE a = x;
  DELETE FROM u WHERE d = x;
  INSERT INTO u (d) SELECT a FROM t WHERE c > x;
  INSERT INTO t (a) VALUES (x), (x + 1) ON CONFLICT (a) DO UPDATE SET c = excluded.c WHERE t.c > 0;
  INSERT INTO t DEFAULT VALUES ON CONFLICT DO NOTHING;
  DELETE FROM t USING u WHERE t.a = u.a RETURNING t.a;
END;
CREATE FUNCTION b5(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC
  UPDATE t SET c = u.d FROM u WHERE t.a = u.a AND u.d > x RETURNING c;
END;
CREATE FUNCTION b6(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC
  WITH w AS (SELECT a FROM t) SELECT count(*) FROM w, u WHERE w.a = u.a;
END;
CREATE FUNCTION b7(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC
  INSERT INTO t AS tt (a, c) VALUES (x, x) ON CONFLICT ON CONSTRAINT t_pkey DO UPDATE SET c = tt.c + 1;
  SELECT 1;
END;
CREATE PROCEDURE p1(x int) LANGUAGE sql BEGIN ATOMIC INSERT INTO t VALUES (x, 'b'); END;
CREATE PROCEDURE p2(x int, INOUT y int) LANGUAGE sql BEGIN ATOMIC SELECT x + y; END;
SELECT pg_get_functiondef(oid) FROM pg_proc WHERE pronamespace = 'sb'::regnamespace ORDER BY proname;
SELECT proname, pg_get_function_sqlbody(oid) FROM pg_proc WHERE pronamespace = 'sb'::regnamespace ORDER BY proname;
SELECT proname, prosrc, prosqlbody IS NOT NULL AS has_body FROM pg_proc WHERE pronamespace = 'sb'::regnamespace ORDER BY proname;
SELECT pg_get_function_sqlbody('pg_catalog.lower(text)'::regprocedure), pg_get_function_sqlbody(0);
ALTER TABLE t RENAME COLUMN c TO cc;
SELECT pg_get_functiondef('b4'::regproc);
SELECT pg_get_functiondef('r2'::regproc);
DROP SCHEMA sb CASCADE;
RESET search_path;
-- sublinks, range-table names, CTEs with DML, positional and composite arguments
CREATE SCHEMA zb;
SET search_path = zb, public;
CREATE TABLE t (a int PRIMARY KEY, b text, c int);
CREATE TABLE u (a int, d int);
CREATE TYPE pair AS (p int, q int);
CREATE FUNCTION e1(x int) RETURNS bigint LANGUAGE sql BEGIN ATOMIC
  DELETE FROM t WHERE a IN (SELECT a FROM u WHERE d = x) AND EXISTS (SELECT 1 FROM t WHERE c = x);
  INSERT INTO t SELECT * FROM t WHERE a = x;
  UPDATE t AS z SET c = 1 WHERE z.a = x;
  SELECT $1 + x FROM u;
  WITH g AS (DELETE FROM u WHERE d = x RETURNING a) SELECT count(*) FROM g;
END;
CREATE FUNCTION e2(int, text) RETURNS text LANGUAGE sql RETURN $2 || $1::text;
CREATE FUNCTION e3(r pair) RETURNS int LANGUAGE sql RETURN (r).p + r.q;
CREATE FUNCTION e4(x int) RETURNS SETOF int LANGUAGE sql BEGIN ATOMIC SELECT a FROM t JOIN u USING (a) WHERE u.d > x ORDER BY 1 LIMIT 3; END;
CREATE FUNCTION e5(x int, OUT y int, OUT z text) LANGUAGE sql BEGIN ATOMIC SELECT x, 'a'; END;
CREATE FUNCTION e6() RETURNS bigint LANGUAGE sql RETURN (SELECT sum(a) FROM t) + (SELECT count(*) FROM u);
CREATE FUNCTION e7(x int) RETURNS int LANGUAGE sql STABLE BEGIN ATOMIC SELECT x; SELECT x + 1; END;
CREATE FUNCTION e8(x int[]) RETURNS int LANGUAGE sql RETURN x[1] + array_length(x, 1);
CREATE FUNCTION e9(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC INSERT INTO u VALUES (x, DEFAULT) RETURNING a, d AS dd; END;
SELECT pg_get_functiondef(oid) FROM pg_proc WHERE pronamespace = 'zb'::regnamespace ORDER BY proname;
SELECT e2(1, 'n'), e3(ROW(1, 2)), e8(ARRAY[4, 5]), e7(1);
DROP TABLE u;
DROP TABLE t;
DROP SCHEMA zb CASCADE;
RESET search_path;
-- a record argument for a composite parameter: only a ROW constructor coerces
CREATE TYPE zpair AS (p int, q int);
CREATE FUNCTION zf(r zpair) RETURNS int LANGUAGE sql RETURN r.p + r.q;
SELECT zf(ROW(1, 2, 3));
SELECT zf(ROW(1));
SELECT zf(ROW('a', 2));
SELECT zf(r) FROM (SELECT ROW(1, 2) AS r) s;
SELECT zf(s) FROM (SELECT 1 AS a, 2 AS b) s;
SELECT zf((1, 2));
DROP FUNCTION zf;
DROP TYPE zpair;
-- RETURN inside BEGIN ATOMIC
CREATE FUNCTION sb_ra(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; RETURN x + 1; END;
CREATE FUNCTION sb_rb(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC RETURN (SELECT x); END;
SELECT pg_get_functiondef('sb_ra'::regproc);
SELECT pg_get_function_sqlbody('sb_rb'::regproc), sb_ra(1), sb_rb(2);
DROP FUNCTION sb_ra, sb_rb;
