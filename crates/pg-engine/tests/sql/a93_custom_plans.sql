-- Custom plans (plancache.c's choose_custom_plan) where PostgreSQL makes them: a SQL
-- function's statement planned per call, PL/pgSQL statements that read variables,
-- EXECUTE ... USING, prepared statements. A custom plan folds the arguments as constants,
-- so a failing constant fails even when no row reaches it.
CREATE TABLE e(x int);
CREATE TABLE t AS SELECT g AS x FROM generate_series(1, 3) g;
-- SQL functions not inlined: scalar, in a select list, in FROM
CREATE FUNCTION f(n int) RETURNS int LANGUAGE sql AS $$ SELECT 1 / n FROM e $$;
SELECT f(0);
SELECT f(0) FROM t;
SELECT f(1) FROM t;
CREATE FUNCTION g(n int) RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 / n FROM e $$;
SELECT * FROM g(0);
SELECT g(0);
CREATE FUNCTION f3(n int) RETURNS SETOF int LANGUAGE sql AS $$ SELECT x / n FROM t $$;
SELECT * FROM f3(1);
SELECT f3(1);
SELECT * FROM f3(0);
-- custom plans go on while they cost less than the generic plan
CREATE FUNCTION f2(n int) RETURNS int LANGUAGE sql AS $$ SELECT 1 / (n - 6) FROM e $$;
SELECT count(f2(g)) FROM generate_series(1, 10) g;
SELECT count(f2(6));
-- after five custom plans a cheaper generic plan is used: no error then
CREATE TABLE k(id int PRIMARY KEY, v int);
INSERT INTO k SELECT g, g FROM generate_series(1, 1000) g;
ANALYZE k;
CREATE FUNCTION fk(n int) RETURNS int LANGUAGE sql AS $$ SELECT 1 / (n - 7) FROM k WHERE id = n + 2000 $$;
SELECT count(fk(g)) FROM generate_series(1, 10) g;
SELECT fk(7);
SET plan_cache_mode = force_custom_plan;
SELECT fk(7);
SET plan_cache_mode = force_generic_plan;
SELECT f(0);
RESET plan_cache_mode;
-- polymorphic arguments
CREATE FUNCTION f4(n anyelement) RETURNS anyelement LANGUAGE sql AS $$ SELECT n FROM t LIMIT 1 $$;
SELECT f4(1), f4('a'::text);
-- PL/pgSQL
CREATE FUNCTION p(n int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r int;
BEGIN
  SELECT 1 / n INTO r FROM e;
  RETURN r;
END $$;
SELECT p(0);
SELECT p(1);
CREATE FUNCTION p2(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN (SELECT 1 / n FROM e);
END $$;
SELECT p2(0);
-- a simple expression is never planned custom
CREATE FUNCTION p3(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  IF n = 0 THEN RETURN 1 / n; END IF; RETURN 0;
END $$;
SELECT p3(1);
CREATE FUNCTION p4(n int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r int := 0;
BEGIN
  FOR i IN 1..8 LOOP
    BEGIN
      SELECT 1 / (n - i) INTO r FROM e;
    EXCEPTION WHEN division_by_zero THEN RAISE NOTICE 'dz at %', i;
    END;
  END LOOP;
  RETURN r;
END $$;
SELECT p4(3);
SELECT p4(7);
SET plan_cache_mode = force_generic_plan;
SELECT p4(3);
SET plan_cache_mode = force_custom_plan;
SELECT p4(7);
RESET plan_cache_mode;
CREATE FUNCTION pk(n int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r int := 0;
BEGIN
  FOR i IN 1..8 LOOP
    BEGIN
      SELECT 1 / (n - i) INTO r FROM k WHERE id = i + 2000;
    EXCEPTION WHEN division_by_zero THEN RAISE NOTICE 'dz at %', i;
    END;
  END LOOP;
  RETURN r;
END $$;
SELECT pk(3);
SELECT pk(7);
DO $$ BEGIN EXECUTE 'SELECT 1 / $1 FROM e' USING 0; END $$;
DO $$ DECLARE n int := 0; BEGIN PERFORM 1 / n FROM e; END $$;
CREATE FUNCTION p5(r t) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN (SELECT 1 / r.x FROM e); END $$;
SELECT p5(ROW(0)::t);
-- prepared statements
PREPARE q(int) AS SELECT 1 / $1 FROM e;
EXECUTE q(0);
SET plan_cache_mode = force_generic_plan;
EXECUTE q(0);
RESET plan_cache_mode;
PREPARE q2(int) AS SELECT * FROM t WHERE x = $1;
EXPLAIN (COSTS OFF) EXECUTE q2(1);
EXECUTE q2(1);
EXECUTE q2(1);
EXECUTE q2(1);
EXECUTE q2(1);
EXECUTE q2(1);
EXPLAIN (COSTS OFF) EXECUTE q2(1);
SELECT name, generic_plans, custom_plans FROM pg_prepared_statements ORDER BY name;
DEALLOCATE ALL;
