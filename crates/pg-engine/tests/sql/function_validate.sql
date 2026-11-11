-- CREATE FUNCTION's body checks (check_function_bodies) run with the function's SET clauses
CREATE SCHEMA fv;
CREATE TABLE fv.tt (a int);
CREATE TYPE fv.ty AS (x int, y text);
CREATE FUNCTION fv_v1() RETURNS bigint LANGUAGE sql SET search_path = fv AS $$ SELECT count(*) FROM tt $$;
SELECT fv_v1();
CREATE FUNCTION fv_v2() RETURNS bigint LANGUAGE sql AS $$ SELECT count(*) FROM tt $$;
CREATE FUNCTION fv_v3() RETURNS int LANGUAGE plpgsql SET search_path = fv, public AS $$
DECLARE v ty; w tt.a%TYPE; r tt%ROWTYPE;
BEGIN v := ROW(1, 'a'); RETURN v.x; END $$;
SELECT fv_v3();
CREATE FUNCTION fv_v4() RETURNS int LANGUAGE plpgsql AS $$
DECLARE v ty;
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_v5() RETURNS bigint LANGUAGE sql SET check_function_bodies = off AS $$ SELECT count(*) FROM nosuch_fv $$;
SELECT proname, proconfig FROM pg_proc WHERE proname = 'fv_v5';
-- a SQL-standard body is analyzed as the statement is, without the SET clauses
CREATE FUNCTION fv_v6() RETURNS bigint LANGUAGE sql SET search_path = fv BEGIN ATOMIC SELECT count(*) FROM tt; END;
CREATE FUNCTION fv_v7() RETURNS int LANGUAGE sql SET work_mem = 'abc' AS $$ SELECT 1 $$;
CREATE FUNCTION fv_v7() RETURNS int LANGUAGE sql SET search_path = fv AS $$ SELECT 1::ty $$;
CREATE FUNCTION fv_v7() RETURNS fv.ty LANGUAGE sql SET search_path = fv AS $$ SELECT ROW(1, 'a')::ty $$;
SELECT fv_v7(), (fv_v7()).y;
SELECT * FROM fv_v7();
SHOW search_path;
CREATE FUNCTION fv_v8() RETURNS int LANGUAGE sql SET datestyle = 'German' AS $$ SELECT extract(day FROM '01.02.2020'::date)::int $$;
SELECT fv_v8();
SET check_function_bodies = off;
CREATE FUNCTION fv_v9() RETURNS int LANGUAGE plpgsql SET check_function_bodies = on AS $$
DECLARE v nosuch_fv;
BEGIN RETURN 1; END $$;
RESET check_function_bodies;
-- PL/pgSQL declarations are resolved when the function is created
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int; v nosuch.col%TYPE;
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int; v pg_class.nosuch%TYPE;
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int; v fv.tt.nosuch%TYPE;
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int; v nosuch%ROWTYPE;
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int;
BEGIN BEGIN DECLARE z nosuch2; BEGIN NULL; END; END; RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int;
BEGIN IF false THEN DECLARE z nosuch3; BEGIN NULL; END; END IF; RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE c CURSOR (x nosuch4) FOR SELECT 1;
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_w1() RETURNS int LANGUAGE plpgsql AS $$
DECLARE v int[]; w varchar(3); u nosuch5[];
BEGIN RETURN 1; END $$;
CREATE FUNCTION fv_w1(p int, OUT q int) LANGUAGE plpgsql AS $$
<<lbl>> DECLARE v int; w lbl.v%TYPE; c p%TYPE; d c%TYPE; e fv_w1.p%TYPE; f q%TYPE;
  r pg_class%ROWTYPE; s pg_class.relname%TYPE; z pg_catalog.int4; t fv.tt%ROWTYPE; u fv.tt.a%TYPE;
BEGIN q := 1; END $$;
SELECT fv_w1(1);
DROP FUNCTION fv_w1;
-- DROP / ALTER of a routine named without arguments
DROP FUNCTION fv_nope;
DROP PROCEDURE fv_nope;
DROP ROUTINE fv.fv_nope;
ALTER FUNCTION fv_nope STABLE;
DROP FUNCTION fv_nope();
DROP FUNCTION IF EXISTS fv_nope;
DROP SCHEMA fv CASCADE;
-- errors in a SQL-standard body point into the statement
CREATE TABLE fv_t1 (a int, b text);
CREATE FUNCTION fv_b() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT nosuch FROM fv_t1; END;
CREATE FUNCTION fv_b() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1 FROM nosuch_t; END;
CREATE FUNCTION fv_b() RETURNS int LANGUAGE sql RETURN nosuch_fn(1);
CREATE FUNCTION fv_b(x int) RETURNS int LANGUAGE sql RETURN x + "Nope";
CREATE FUNCTION fv_b(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT a FROM fv_t1 WHERE b = 1; END;
CREATE FUNCTION fv_b(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; INSERT INTO fv_t1 VALUES ('x', 'y'); END;
CREATE FUNCTION fv_b(x int) RETURNS int
  LANGUAGE sql
  BEGIN ATOMIC
    SELECT 1;
    SELECT a FROM fv_t1 WHERE a = 1 + 'z';
  END;
-- prosrc is the body as written
CREATE FUNCTION fv_s1() RETURNS int LANGUAGE sql AS $$ SELECT 1; $$;
CREATE FUNCTION fv_s2() RETURNS int LANGUAGE sql AS '
  SELECT 1;
  SELECT 2
';
CREATE FUNCTION fv_s3() RETURNS bigint LANGUAGE sql AS $$ SELECT count(*) FROM fv_t1 $$;
SELECT proname, prosrc, prosqlbody IS NULL AS no_sqlbody FROM pg_proc WHERE proname LIKE 'fv_s%' ORDER BY 1;
SELECT pg_get_functiondef('fv_s1'::regproc);
SELECT pg_get_functiondef('fv_s2'::regproc);
SELECT fv_s1(), fv_s2(), fv_s3();
DROP FUNCTION fv_s1, fv_s2, fv_s3;
DROP TABLE fv_t1;
