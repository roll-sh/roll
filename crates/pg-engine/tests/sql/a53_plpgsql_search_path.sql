-- PL/pgSQL, SQL functions, DO blocks and prepared statements bind each statement under the
-- search path as it is when the statement runs (plancache's search_path check), and a
-- function's SET search_path holds while it runs, restored on exit and on error.
DROP ROLE IF EXISTS regress_a53_sp_user;
DROP ROLE IF EXISTS regress_a53_sp_owner;
CREATE SCHEMA s1;
CREATE SCHEMA s2;
CREATE TABLE s1.t(x int);
INSERT INTO s1.t VALUES (1);
CREATE TABLE s2.t(x int);
INSERT INTO s2.t VALUES (2);
CREATE FUNCTION s1.who() RETURNS text LANGUAGE sql AS 'SELECT ''s1''';
CREATE FUNCTION s2.who() RETURNS text LANGUAGE sql AS 'SELECT ''s2''';
SET search_path = s1, public;

-- set_config, SET and EXECUTE 'SET' affect the statements after them
CREATE FUNCTION public.f1() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text;
BEGIN
  SELECT x::text INTO r FROM t; RAISE NOTICE 'a %', r;
  PERFORM set_config('search_path', 's2', true);
  SELECT x::text INTO r FROM t; RAISE NOTICE 'b % %', r, who();
  SET search_path = s1;
  RAISE NOTICE 'c % %', (SELECT x FROM t), who();
  EXECUTE 'SET search_path = s2';
  EXECUTE 'SELECT x::text FROM t' INTO r; RAISE NOTICE 'd %', r;
  RETURN current_setting('search_path');
END $$;
BEGIN;
SELECT public.f1();
SHOW search_path;
ROLLBACK;
SHOW search_path;

-- the same statement, its plan cached, under a path that changes every iteration
CREATE FUNCTION public.f2() RETURNS void LANGUAGE plpgsql AS $$
DECLARE n int;
BEGIN
  FOR i IN 1..4 LOOP
    PERFORM set_config('search_path', CASE WHEN i % 2 = 0 THEN 's1' ELSE 's2' END, true);
    SELECT x INTO n FROM t;
    RAISE NOTICE 'i=% t=% who=% expr=%', i, n, who(), (SELECT x FROM t);
  END LOOP;
END $$;
BEGIN;
SELECT public.f2();
SELECT public.f2();
ROLLBACK;

-- a SQL function plans each statement when it is reached
CREATE FUNCTION public.f3() RETURNS int LANGUAGE sql AS $$
  SELECT set_config('search_path', 's2', true);
  SELECT x FROM t
$$;
BEGIN;
SELECT public.f3();
SHOW search_path;
ROLLBACK;

-- a SET clause holds while the function runs; what it SETs itself stays
CREATE FUNCTION public.f4(fail bool) RETURNS text LANGUAGE plpgsql SET search_path = s2 AS $$
BEGIN
  RAISE NOTICE 'in f4 % % %', (SELECT x FROM t), who(), current_setting('search_path');
  IF fail THEN RAISE EXCEPTION 'boom'; END IF;
  PERFORM set_config('search_path', 's1, public', false);
  RETURN who() || (SELECT x FROM t);
END $$;
SELECT public.f4(true);
SHOW search_path;
SELECT who(), (SELECT x FROM t);
SELECT public.f4(false), who();
SHOW search_path;
SET search_path = s1, public;

-- caught inside: the setting is restored with the subtransaction, the SET clause after
CREATE FUNCTION public.f4b() RETURNS text LANGUAGE plpgsql SET search_path = s2 AS $$
BEGIN
  BEGIN
    PERFORM set_config('search_path', 's1', true);
    RAISE NOTICE 'inner %', who();
    PERFORM 1 / 0;
  EXCEPTION WHEN division_by_zero THEN
    RAISE NOTICE 'caught % %', who(), current_setting('search_path');
  END;
  RETURN who();
END $$;
SELECT public.f4b(), who();

-- nested calls: a callee's SET LOCAL is seen by its caller's next statements
CREATE FUNCTION public.inner_set(p text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN PERFORM set_config('search_path', p, true); END $$;
CREATE FUNCTION public.outer_f() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'before %', who();
  PERFORM public.inner_set('s2');
  RAISE NOTICE 'after %', who();
  RETURN (SELECT x FROM t)::text || public.mid_f();
END $$;
CREATE FUNCTION public.mid_f() RETURNS text LANGUAGE plpgsql SET search_path = s1 AS $$
BEGIN
  RAISE NOTICE 'mid %', who();
  PERFORM public.inner_set('s2');
  RETURN who();
END $$;
BEGIN;
SELECT public.outer_f();
SHOW search_path;
ROLLBACK;

-- DO blocks
BEGIN;
DO $$
BEGIN
  RAISE NOTICE 'do1 %', who();
  PERFORM set_config('search_path', 's2', true);
  RAISE NOTICE 'do2 % %', who(), (SELECT x FROM t);
END $$;
ROLLBACK;

-- prepared statements are bound again under another path
PREPARE p AS SELECT x, who() FROM t;
EXECUTE p;
SET search_path = s2;
EXECUTE p;
SET search_path = s1, public;
EXECUTE p;
DEALLOCATE p;

-- other settings a routine changes are seen by its next statements
CREATE FUNCTION public.f6() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '%', '2020-03-04'::date;
  SET LOCAL datestyle = 'German';
  RAISE NOTICE '%', '2020-03-04'::date;
  RETURN '2020-03-04'::date::text;
END $$;
BEGIN;
SELECT public.f6();
ROLLBACK;

-- an extension-script-style guard: with pg_catalog first, a user's = (oid, regclass) in a
-- schema on the caller's path is not what the block's later comparisons call
CREATE FUNCTION s1.cmp(oid, regclass) RETURNS boolean LANGUAGE plpgsql AS
'BEGIN RAISE EXCEPTION ''overloaded cmp() called by %'', current_user; END';
CREATE OPERATOR s1.= (LEFTARG = oid, RIGHTARG = regclass, PROCEDURE = s1.cmp);
DO $$
DECLARE
  old_path pg_catalog.text := pg_catalog.current_setting('search_path');
  n int;
BEGIN
  PERFORM pg_catalog.set_config('search_path', 'pg_catalog, pg_temp', true);
  SELECT count(*) INTO n FROM pg_catalog.pg_class WHERE oid = 'pg_catalog.pg_class'::pg_catalog.regclass;
  RAISE NOTICE 'safe %', n;
  PERFORM pg_catalog.set_config('search_path', old_path, true);
  SELECT count(*) INTO n FROM pg_catalog.pg_class WHERE oid = 'pg_catalog.pg_class'::pg_catalog.regclass;
END $$;
SHOW search_path;
DROP OPERATOR s1.= (oid, regclass);

-- SECURITY DEFINER with its own path; schemas the current role may not use are skipped
CREATE ROLE regress_a53_sp_owner;
CREATE ROLE regress_a53_sp_user;
GRANT USAGE ON SCHEMA s1 TO regress_a53_sp_owner, regress_a53_sp_user;
GRANT SELECT ON s1.t TO regress_a53_sp_owner, regress_a53_sp_user;
GRANT USAGE ON SCHEMA s2 TO regress_a53_sp_owner;
GRANT SELECT ON s2.t TO regress_a53_sp_owner;
CREATE FUNCTION public.def_f() RETURNS text LANGUAGE plpgsql SECURITY DEFINER SET search_path = s2, s1 AS $$
BEGIN
  RETURN current_user || ' ' || who() || ' ' || (SELECT x FROM t)::text;
END $$;
ALTER FUNCTION public.def_f() OWNER TO regress_a53_sp_owner;
CREATE FUNCTION public.inv_f() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  PERFORM set_config('search_path', 's2, s1', true);
  RETURN current_user || ' ' || who() || ' ' || (SELECT x FROM t)::text || ' ' || public.def_f();
END $$;
SET ROLE regress_a53_sp_user;
BEGIN;
SELECT public.inv_f();
SHOW search_path;
ROLLBACK;
RESET ROLE;

-- CREATE SCHEMA's elements see the new schema prepended to search_path
CREATE FUNCTION public.showpath() RETURNS int LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'path=%', current_setting('search_path'); RETURN 10; END $$;
CREATE SCHEMA s3
  CREATE TABLE p(i int) PARTITION BY RANGE (i)
  CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (public.showpath());
SHOW search_path;
SELECT relname, relnamespace::regnamespace FROM pg_class WHERE relname IN ('p', 'p1') ORDER BY 1;

-- DDL a function a partition bound calls stays made
CREATE FUNCTION public.mk() RETURNS int LANGUAGE plpgsql AS $$
BEGIN CREATE TABLE public.made_by_bound(i int); RETURN 5; END $$;
CREATE TABLE public.pt(i int) PARTITION BY RANGE (i);
CREATE TABLE public.pt1 PARTITION OF public.pt FOR VALUES FROM (1) TO (public.mk());
SELECT relname FROM pg_class WHERE relname IN ('pt1', 'made_by_bound') ORDER BY 1;
DROP TABLE public.pt, public.made_by_bound;

-- an index build is a security-restricted operation: what its expressions' functions SET
-- doesn't outlive it, and names in the statement resolve as it began
CREATE FUNCTION public.setter() RETURNS bool VOLATILE LANGUAGE sql AS $$SET search_path = s1; SELECT true$$;
CREATE FUNCTION s2.konst() RETURNS bool IMMUTABLE LANGUAGE sql AS $$SELECT public.setter()$$;
CREATE FUNCTION s2.ix(text, bool) RETURNS text IMMUTABLE LANGUAGE sql AS $$SELECT $1$$;
CREATE COLLATION s1.coll (LOCALE = "C");
CREATE TABLE s2.x (y text);
INSERT INTO s2.x VALUES ('foo'), ('bar');
RESET search_path;
CREATE INDEX x_y ON s2.x ((s2.ix(y, s2.konst())));
SHOW search_path;
ALTER TABLE s2.x ADD CONSTRAINT x_excl EXCLUDE USING btree ((s2.ix(y, s2.konst())) WITH =);
SHOW search_path;
ALTER TABLE s2.x ADD CONSTRAINT x_bad EXCLUDE USING btree ((s2.ix(y, s2.konst())) COLLATE coll WITH =);
SHOW search_path;
SET search_path = s1, public;

DROP SCHEMA s1, s2, s3 CASCADE;
DROP FUNCTION public.f1(), public.f2(), public.f3(), public.f4(bool), public.f4b(),
  public.inner_set(text), public.outer_f(), public.mid_f(), public.f6(), public.def_f(),
  public.inv_f(), public.showpath(), public.mk(), public.setter();
DROP ROLE regress_a53_sp_user;
DROP ROLE regress_a53_sp_owner;
