-- A SQL function's statements run in their caller's executor context, bound by binders
-- of their own: their uncorrelated sub-selects (InitPlans) must not share cached results
-- with the caller's or with another call's.
CREATE TABLE t (x int);
INSERT INTO t VALUES (1), (2);
CREATE TABLE u (y int);
INSERT INTO u VALUES (10), (20), (30);
-- The argument is a constant in the function's statement: each call's sub-select differs.
CREATE FUNCTION g(int) RETURNS int LANGUAGE sql AS $$ SELECT (SELECT $1 * 10) $$;
SELECT x, g(x) FROM generate_series(1, 3) x ORDER BY x;
SELECT g(1), g(2), g(3);
-- The caller's sub-select and the function's.
CREATE FUNCTION f() RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT (SELECT count(*) FROM u) $$;
SELECT (SELECT min(x) FROM t) AS a, f() AS b;
SELECT f() AS b, (SELECT min(x) FROM t) AS a;
SELECT (SELECT max(y) FROM u) AS a, f() AS b, (SELECT min(x) FROM t) AS c;
-- A statement of several, each with a sub-select of its own.
CREATE FUNCTION h(int) RETURNS int LANGUAGE sql AS $$
  INSERT INTO t SELECT (SELECT $1 + 100);
  SELECT (SELECT max(x) FROM t) + (SELECT $1);
$$;
SELECT h(5), (SELECT count(*) FROM t);
SELECT h(7);
SELECT x FROM t ORDER BY x;
-- In a WHERE clause, per row.
SELECT y FROM u WHERE g(y / 10) = y ORDER BY y;
-- A procedure's CALL arguments in a SQL function.
CREATE TABLE log (v int);
CREATE PROCEDURE p(v int) LANGUAGE sql AS $$ INSERT INTO log VALUES (v) $$;
CREATE FUNCTION c(int) RETURNS int LANGUAGE sql AS $$ CALL p($1 * 2); SELECT (SELECT count(*)::int FROM log) $$;
SELECT c(1), c(2), (SELECT 99);
SELECT v FROM log ORDER BY v;
