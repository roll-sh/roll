-- Found by the differential tester (tests/difftest.rs). A function in FROM returning one
-- OUT column (RETURNS TABLE (x int), OUT y) or a one-field composite names the column after
-- it, whatever the alias (chooseScalarFunctionAlias); only a plain scalar takes the alias.
-- A scalar function's whole-row reference is its value.
CREATE TABLE t1 (id int4 PRIMARY KEY, d json);
INSERT INTO t1 VALUES (1, '1'), (5, '2');
CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id FROM t1 WHERE id <= lim;
END
$$;
CREATE FUNCTION srf1(lim int) RETURNS TABLE(x int) LANGUAGE sql AS $$ SELECT id FROM t1 WHERE id <= lim $$;
SELECT * FROM srf0(9) s2;
SELECT * FROM srf1(9) s2;
SELECT s2.x FROM srf1(9) s2;
SELECT * FROM srf0(9);
SELECT s2.x FROM srf0(9) s2;
SELECT s2 FROM srf0(9) s2;
SELECT s2 FROM srf1(9) s2;
SELECT s2.* FROM srf1(9) s2;
SELECT s IS NULL FROM srf1(NULL) s;
SELECT to_jsonb(s) FROM srf1(9) s;
CREATE TYPE one AS (f int);
CREATE FUNCTION c1() RETURNS SETOF one LANGUAGE sql AS $$ SELECT id FROM t1 $$;
CREATE FUNCTION c2() RETURNS one LANGUAGE sql AS $$ SELECT id FROM t1 LIMIT 1 $$;
CREATE FUNCTION o1(a int, OUT y int) LANGUAGE sql AS $$ SELECT a + 1 $$;
CREATE FUNCTION o2(a int, OUT y int) LANGUAGE plpgsql AS $$ BEGIN y := a; END $$;
CREATE FUNCTION p1(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$;
CREATE FUNCTION s1(a int) RETURNS SETOF int LANGUAGE sql AS $$ SELECT a $$;
SELECT * FROM c1() s;
SELECT s, pg_typeof(s) FROM c1() s;
SELECT * FROM c2() s;
SELECT * FROM o1(1) s;
SELECT s FROM o1(1) s;
SELECT * FROM o2(1) s;
SELECT * FROM p1(1) s;
SELECT s FROM p1(1) s;
SELECT * FROM s1(1) s;
SELECT * FROM o1(1);
SELECT * FROM p1(1);
SELECT * FROM o1(1) s(z);
SELECT s FROM o1(1) s(z);
SELECT * FROM c1() WITH ORDINALITY s;
SELECT * FROM srf0(9) WITH ORDINALITY s;
SELECT * FROM p1(1) WITH ORDINALITY s;
SELECT s FROM generate_series(1, 2) s;
SELECT s FROM generate_series(1, 2) s(g);
SELECT s FROM upper('a') s;
DROP TYPE one CASCADE;
