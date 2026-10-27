-- Recursive SQL functions. inline_function never inlines a function into its own inlined
-- body (active_fns): that call runs when executed, and each call plans its body afresh,
-- inlining one more level.
CREATE FUNCTION rec_f(n int) RETURNS int LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN 0 ELSE rec_f(n - 1) + 1 END';
SELECT rec_f(0), rec_f(1), rec_f(2), rec_f(10), rec_f(1000);
SELECT g, rec_f(g) FROM generate_series(0, 5) g;
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_f(5);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_f(g) FROM generate_series(1, 3) g;
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_f(g) + rec_f(g + 1) FROM generate_series(1, 3) g;

-- Immutable: evaluated over constants while planning.
CREATE FUNCTION rec_fi(n int) RETURNS int IMMUTABLE LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN 0 ELSE rec_fi(n - 1) + 1 END';
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_fi(5);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_fi(g) FROM generate_series(1, 3) g;
SELECT rec_fi(20), rec_fi(g) FROM generate_series(3, 4) g;

-- Not inlinable (a FROM clause): every level is a call.
CREATE FUNCTION rec_nf(n int) RETURNS int LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN 0 ELSE rec_nf(n - 1) + 1 END FROM (VALUES (1)) v';
SELECT rec_nf(0), rec_nf(7), rec_nf(500);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_nf(5);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_nf(5) FROM generate_series(1, 3) g;

-- Through a scalar subquery.
CREATE FUNCTION rec_sq(n int) RETURNS int LANGUAGE sql
  AS 'SELECT n + coalesce((SELECT rec_sq(n - 1) FROM (VALUES (1)) v WHERE n > 0), 0)';
SELECT rec_sq(10), rec_sq(300);

-- Strict, with a construct that isn't strict in its body (so never inlined).
CREATE FUNCTION rec_st(n int) RETURNS int STRICT LANGUAGE sql
  AS 'SELECT coalesce(rec_st(nullif(n, 0) - 1), 0) + 1';
SELECT rec_st(0), rec_st(5), rec_st(NULL);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_st(g) FROM generate_series(1, 2) g;

-- Mutual recursion: each inlines the other once.
CREATE FUNCTION rec_ev(n int) RETURNS bool LANGUAGE sql AS 'SELECT true';
CREATE FUNCTION rec_od(n int) RETURNS bool LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN false ELSE rec_ev(n - 1) END';
CREATE OR REPLACE FUNCTION rec_ev(n int) RETURNS bool LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN true ELSE rec_od(n - 1) END';
SELECT g, rec_ev(g), rec_od(g) FROM generate_series(0, 6) g;
SELECT rec_ev(1001), rec_od(1001);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_ev(g) FROM generate_series(1, 2) g;
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_od(4);

-- Polymorphic and variadic.
CREATE FUNCTION rec_poly(n int, x anyelement) RETURNS anyelement LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN x ELSE rec_poly(n - 1, x) END';
SELECT rec_poly(5, 'a'::text), rec_poly(3, 2.5), rec_poly(4, ARRAY[1, 2]), rec_poly(0, NULL::date);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_poly(g, 'a'::text) FROM generate_series(1, 2) g;
CREATE FUNCTION rec_arr(n int, VARIADIC xs int[]) RETURNS int LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN array_length(xs, 1) ELSE rec_arr(n - 1, VARIADIC xs || n) END';
SELECT rec_arr(3, 10, 20);

-- SQL calling PL/pgSQL calling SQL: the SQL function is inlined into each PL/pgSQL
-- statement.
CREATE FUNCTION rec_sp(n int) RETURNS int LANGUAGE sql AS 'SELECT 0';
CREATE FUNCTION rec_pp(n int) RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN rec_sp(n); END';
CREATE OR REPLACE FUNCTION rec_sp(n int) RETURNS int LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN 0 ELSE rec_pp(n - 1) + 1 END';
SELECT rec_sp(10), rec_pp(10);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_sp(g) FROM generate_series(1, 2) g;

-- An error deep in the recursion: one CONTEXT line per call.
CREATE FUNCTION rec_err(n int) RETURNS int LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN 1 / n ELSE rec_err(n - 1) END';
SELECT rec_err(0);
SELECT rec_err(1);
CREATE FUNCTION rec_err2(n int) RETURNS int LANGUAGE sql
  AS 'SELECT CASE WHEN n = 0 THEN 1 / n ELSE rec_err2(n - 1) END FROM (VALUES (1)) v';
SELECT rec_err2(3);
CREATE FUNCTION rec_perr(n int) RETURNS int LANGUAGE plpgsql
  AS 'BEGIN IF n = 0 THEN RETURN 100; END IF; RETURN rec_serr(n - 1) + 1; END';
CREATE FUNCTION rec_serr(n int) RETURNS int LANGUAGE sql AS 'SELECT rec_perr(n) FROM (VALUES (1)) v';
SELECT rec_perr(0), rec_perr(5), rec_serr(5);

-- Set-returning: volatile ones run as function scans; a call in the select list.
CREATE FUNCTION rec_srf(n int) RETURNS SETOF int LANGUAGE sql
  AS 'SELECT n UNION ALL SELECT * FROM rec_srf(n - 1) WHERE n > 0';
SELECT * FROM rec_srf(3);
SELECT rec_srf(2);
SELECT count(*), sum(x) FROM rec_srf(400) x;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM rec_srf(3);
SELECT g, x FROM generate_series(1, 2) g, rec_srf(g) x;
CREATE FUNCTION rec_tab(n int) RETURNS TABLE (a int, b text) LANGUAGE sql
  AS 'SELECT n, n::text UNION ALL SELECT * FROM rec_tab(n - 1) WHERE n > 0';
SELECT * FROM rec_tab(2);
SELECT (rec_tab(1)).*;
CREATE FUNCTION rec_srf2(n int) RETURNS SETOF int LANGUAGE sql
  AS 'SELECT n UNION ALL SELECT rec_srf2(n - 1) WHERE n > 0';
SELECT * FROM rec_srf2(3);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM rec_srf2(3);
-- Stable: Postgres inlines it in FROM level by level, until a level's `n > 0` is false.
CREATE FUNCTION rec_srfi(n int) RETURNS SETOF int STABLE LANGUAGE sql
  AS 'SELECT n UNION ALL SELECT * FROM rec_srfi(n - 1) WHERE n > 0';
SELECT * FROM rec_srfi(3);
SELECT count(*) FROM rec_srfi(50);
CREATE FUNCTION rec_srfb(n int) RETURNS SETOF int STABLE LANGUAGE sql
  AS 'SELECT * FROM rec_srfb(n - 1) WHERE n > 0 UNION ALL SELECT n';
SELECT * FROM rec_srfb(3);

-- A void function recursing.
CREATE TABLE rec_log (n int);
CREATE FUNCTION rec_void(n int) RETURNS void LANGUAGE sql
  AS 'INSERT INTO rec_log SELECT 0 WHERE n > 0 AND rec_void(n - 1) IS NULL; INSERT INTO rec_log VALUES (n)';
SELECT rec_void(3);
SELECT * FROM rec_log ORDER BY n;

-- A SQL-standard body.
CREATE FUNCTION rec_std(n int) RETURNS int LANGUAGE sql RETURN 0;
CREATE OR REPLACE FUNCTION rec_std(n int) RETURNS int LANGUAGE sql
  RETURN CASE WHEN n = 0 THEN 0 ELSE rec_std(n - 1) + 2 END;
SELECT rec_std(6);
EXPLAIN (VERBOSE, COSTS OFF) SELECT rec_std(g) FROM generate_series(1, 2) g;

-- In a view, a default and a check constraint.
CREATE VIEW rec_v AS SELECT g, rec_f(g) AS r FROM generate_series(1, 3) g;
SELECT * FROM rec_v;
SELECT pg_get_viewdef('rec_v');
CREATE TABLE rec_t (a int DEFAULT rec_f(4), b int CHECK (rec_f(b) < 100));
INSERT INTO rec_t (b) VALUES (5);
INSERT INTO rec_t (b) VALUES (500);
SELECT * FROM rec_t;

DROP VIEW rec_v;
DROP TABLE rec_t, rec_log;
DROP FUNCTION rec_f, rec_fi, rec_nf, rec_sq, rec_st, rec_ev, rec_od, rec_poly, rec_arr,
  rec_sp, rec_pp, rec_err, rec_err2, rec_perr, rec_serr, rec_srf, rec_tab, rec_srf2,
  rec_srfi, rec_srfb, rec_void, rec_std;
