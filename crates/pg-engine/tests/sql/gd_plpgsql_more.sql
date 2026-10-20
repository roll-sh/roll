-- PL/pgSQL: $n and keyword-named variables as targets, made casts on RETURN, CONSTANT cursors,
-- composite RETURN checks, RETURN QUERY of SELECT INTO, FOREACH SLICE, multi-dimensional
-- assignments, variables inside FROM items, transition tables as targets.
CREATE FUNCTION gd_pl_out(out int, out int) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN
  $1 := -1;
  $2 := -2;
  RETURN NEXT;
END $$;
SELECT * FROM gd_pl_out();
CREATE FUNCTION gd_pl_kw() RETURNS int LANGUAGE plpgsql AS $$
DECLARE
  return int := 42;
BEGIN
  return := return + 1;
  RETURN return;
END $$;
SELECT gd_pl_kw();
CREATE FUNCTION gd_pl_todate(int) RETURNS date LANGUAGE sql IMMUTABLE STRICT AS 'SELECT $1::text::date';
CREATE CAST (int AS date) WITH FUNCTION gd_pl_todate(int) AS ASSIGNMENT;
CREATE FUNCTION gd_pl_castret(int) RETURNS date LANGUAGE plpgsql AS 'BEGIN RETURN $1; END';
SELECT gd_pl_castret(20150717);
SELECT gd_pl_castret(-1);
DROP FUNCTION gd_pl_castret(int);
DROP FUNCTION gd_pl_todate(int) CASCADE;
CREATE TABLE gd_pl_rc (a int);
CREATE FUNCTION gd_pl_constcur() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE
  rc CONSTANT refcursor;
BEGIN
  OPEN rc FOR SELECT a FROM gd_pl_rc;
  RETURN rc;
END $$;
SELECT gd_pl_constcur();
DROP FUNCTION gd_pl_constcur();
CREATE TYPE gd_pl_ct AS (x int, y varchar);
CREATE FUNCTION gd_pl_comp() RETURNS gd_pl_ct LANGUAGE plpgsql AS 'BEGIN RETURN (1, ''hello''); END';
SELECT gd_pl_comp();
CREATE OR REPLACE FUNCTION gd_pl_comp() RETURNS gd_pl_ct LANGUAGE plpgsql AS 'BEGIN RETURN (1, ''hello''::varchar); END';
SELECT gd_pl_comp();
CREATE OR REPLACE FUNCTION gd_pl_comp() RETURNS gd_pl_ct LANGUAGE plpgsql AS 'BEGIN RETURN 1 + 1; END';
SELECT gd_pl_comp();
DROP FUNCTION gd_pl_comp();
CREATE FUNCTION gd_pl_rq() RETURNS SETOF int LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT 10 INTO gd_pl_nosuch;
END $$;
SELECT * FROM gd_pl_rq();
DROP FUNCTION gd_pl_rq();
-- FOREACH
CREATE FUNCTION gd_pl_fe(anyarray) RETURNS void LANGUAGE plpgsql AS $$
DECLARE x int[];
BEGIN
  FOREACH x SLICE 2 IN ARRAY $1 LOOP
    RAISE NOTICE '%', x;
  END LOOP;
END $$;
SELECT gd_pl_fe(ARRAY[1, 2, 3, 4]);
SELECT gd_pl_fe(ARRAY[[1, 2], [3, 4]]);
SELECT gd_pl_fe(ARRAY[[[1, 2]], [[3, 4]]]);
CREATE OR REPLACE FUNCTION gd_pl_fe(anyarray) RETURNS void LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  FOREACH x SLICE 1 IN ARRAY $1 LOOP
    RAISE NOTICE '%', x;
  END LOOP;
END $$;
SELECT gd_pl_fe(ARRAY[1, 2, 3, 4]);
CREATE OR REPLACE FUNCTION gd_pl_fe(anyarray) RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  FOREACH r IN ARRAY $1 LOOP
    RAISE NOTICE '%', r;
  END LOOP;
END $$;
SELECT gd_pl_fe(ARRAY[(10, 'a'), (40, 'b')]::gd_pl_ct[]);
DROP FUNCTION gd_pl_fe(anyarray);
DROP TYPE gd_pl_ct;
CREATE FUNCTION gd_pl_md() RETURNS text[] LANGUAGE plpgsql AS $$
DECLARE
  arr text[];
  i int := 1;
BEGIN
  arr := array[array['foo', 'bar'], array['baz', 'quux']];
  arr[(SELECT i)][(SELECT i + 1)] := (SELECT 'fool');
  RETURN arr;
END $$;
SELECT gd_pl_md();
DROP FUNCTION gd_pl_md();
-- variables inside FROM items
CREATE FUNCTION gd_pl_from() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  a int := 5;
  b int := 7;
  n int;
  r record;
  c CURSOR (r1 int, r2 int) FOR SELECT * FROM generate_series(r1, r2) i;
BEGIN
  SELECT count(*) INTO n FROM generate_series(a, b) i;
  RAISE NOTICE 'count %', n;
  SELECT x INTO n FROM (SELECT a + 100 AS x) s;
  RAISE NOTICE 'sub %', n;
  FOR r IN c(a, b) LOOP
    RAISE NOTICE 'cursor %', r.i;
  END LOOP;
END $$;
SELECT gd_pl_from();
DROP FUNCTION gd_pl_from();
-- a transition table isn't a target; a schema-qualified regclass names its schema
CREATE TABLE gd_pl_tt (a int);
CREATE FUNCTION gd_pl_ttf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO dx VALUES (1);
  RETURN NULL;
END $$;
CREATE TRIGGER gd_pl_ttt AFTER INSERT ON gd_pl_tt REFERENCING NEW TABLE AS dx
  FOR EACH STATEMENT EXECUTE FUNCTION gd_pl_ttf();
INSERT INTO gd_pl_tt VALUES (1);
SELECT 'gd_pl_nosch.stuff'::regclass;
SELECT 'gd_pl_nosch.stuff'::text::regclass;
DROP TABLE gd_pl_tt;
DROP FUNCTION gd_pl_ttf(), gd_pl_out(), gd_pl_kw();
DROP TABLE gd_pl_rc;
