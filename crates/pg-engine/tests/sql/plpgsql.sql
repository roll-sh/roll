-- DO blocks
DO $$ BEGIN END $$;
DO $$ BEGIN RAISE NOTICE 'hello % and %', 1, 'x'; END $$;
DO $$ BEGIN RAISE NOTICE 'percent %% and null %', NULL; END $$;
DO $$ BEGIN RAISE INFO 'info'; RAISE WARNING 'warn %', 2; RAISE LOG 'not shown'; RAISE DEBUG 'not shown'; END $$;
DO $$
DECLARE x int := 2; s text; c CONSTANT int := 10; n numeric NOT NULL DEFAULT 1.5;
BEGIN
  FOR i IN 1..3 LOOP x := x * i; END LOOP;
  IF x > 10 THEN s := 'big'; ELSIF x > 5 THEN s := 'mid'; ELSE s := 'small'; END IF;
  RAISE NOTICE 'x=% s=% c=% n=%', x, s, c, n;
END $$;
DO language plpgsql $$ BEGIN RAISE NOTICE 'lang first'; END $$;

-- loops
DO $$
DECLARE i int := 0; total int := 0;
BEGIN
  LOOP i := i + 1; EXIT WHEN i > 5; CONTINUE WHEN i = 3; total := total + i; END LOOP;
  RAISE NOTICE 'loop total %', total;
  WHILE i > 0 LOOP i := i - 2; END LOOP;
  RAISE NOTICE 'while %', i;
  FOR j IN REVERSE 10..1 BY 3 LOOP RAISE NOTICE 'rev %', j; END LOOP;
  FOR j IN 1..0 LOOP RAISE NOTICE 'never'; END LOOP;
  <<outer>> FOR a IN 1..3 LOOP
    FOR b IN 1..3 LOOP
      CONTINUE outer WHEN b = 2;
      EXIT outer WHEN a = 3;
      RAISE NOTICE 'a=% b=%', a, b;
    END LOOP;
  END LOOP outer;
  FOREACH i IN ARRAY ARRAY[5,6,7] LOOP total := total + i; END LOOP;
  RAISE NOTICE 'foreach %', total;
END $$;

-- blocks, labels, shadowing
DO $$
<<top>>
DECLARE v int := 1;
BEGIN
  DECLARE v int := 2;
  BEGIN
    RAISE NOTICE 'inner % outer %', v, top.v;
    top.v := 5;
  END;
  RAISE NOTICE 'after %', v;
END top $$;

-- CASE statement
DO $$
DECLARE r text;
BEGIN
  FOR i IN 1..4 LOOP
    CASE i WHEN 1, 2 THEN r := 'low'; WHEN 3 THEN r := 'three'; ELSE r := 'other'; END CASE;
    RAISE NOTICE '% %', i, r;
  END LOOP;
  CASE WHEN r = 'other' THEN RAISE NOTICE 'searched'; END CASE;
END $$;
DO $$ BEGIN CASE 5 WHEN 1 THEN NULL; END CASE; END $$;

-- SQL in PL/pgSQL
CREATE TABLE t (id int PRIMARY KEY, v text);
DO $$
DECLARE n int; r record; total int := 0; rc bigint;
BEGIN
  FOR i IN 1..5 LOOP INSERT INTO t VALUES (i, 'v' || i); END LOOP;
  GET DIAGNOSTICS rc = ROW_COUNT;
  RAISE NOTICE 'row_count %', rc;
  SELECT count(*) INTO n FROM t;
  RAISE NOTICE 'count % found %', n, FOUND;
  SELECT * INTO r FROM t WHERE id = 3;
  RAISE NOTICE 'r.id=% r.v=%', r.id, r.v;
  SELECT * INTO r FROM t WHERE id = 99;
  RAISE NOTICE 'missing found=% r.v=%', FOUND, r.v;
  FOR r IN SELECT * FROM t ORDER BY id DESC LOOP total := total + r.id; END LOOP;
  RAISE NOTICE 'sum %', total;
  UPDATE t SET v = upper(v) WHERE id <= 2;
  GET DIAGNOSTICS rc = ROW_COUNT;
  RAISE NOTICE 'updated % found %', rc, FOUND;
  DELETE FROM t WHERE id = 42;
  RAISE NOTICE 'delete found %', FOUND;
  PERFORM * FROM t WHERE id = 1;
  RAISE NOTICE 'perform found %', FOUND;
  INSERT INTO t VALUES (6, 'six') RETURNING v INTO r;
  RAISE NOTICE 'returning %', r.v;
END $$;
SELECT * FROM t ORDER BY id;
DO $$ DECLARE n int; BEGIN SELECT id INTO STRICT n FROM t WHERE id > 100; END $$;
DO $$ DECLARE n int; BEGIN SELECT id INTO STRICT n FROM t; END $$;
DO $$ BEGIN SELECT 1; END $$;
DO $$ DECLARE a int; b text; BEGIN SELECT id, v INTO a, b FROM t WHERE id = 2; RAISE NOTICE '% %', a, b; END $$;

-- dynamic SQL
DO $$
DECLARE n int; tbl text := 't';
BEGIN
  EXECUTE 'SELECT count(*) FROM ' || quote_ident(tbl) INTO n;
  RAISE NOTICE 'dyn count %', n;
  EXECUTE 'SELECT v FROM t WHERE id = $1' INTO tbl USING 3;
  RAISE NOTICE 'dyn using %', tbl;
  EXECUTE format('CREATE TABLE %I (x int)', 'dyn_' || n);
  EXECUTE 'INSERT INTO dyn_6 VALUES ($1), ($2)' USING 1, 2;
  FOR n IN EXECUTE 'SELECT x FROM dyn_6 ORDER BY x' LOOP RAISE NOTICE 'dyn row %', n; END LOOP;
END $$;
SELECT * FROM dyn_6 ORDER BY x;

-- DDL inside DO (migrations)
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 't' AND column_name = 'extra') THEN
    ALTER TABLE t ADD COLUMN extra int DEFAULT 0;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'mood') THEN
    CREATE TYPE mood AS ENUM ('sad', 'happy');
  END IF;
END $$;
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'mood') THEN
    CREATE TYPE mood AS ENUM ('sad', 'happy');
  END IF;
END $$;
SELECT id, extra FROM t WHERE id = 1;

-- exceptions
DO $$
BEGIN
  INSERT INTO t VALUES (1, 'dup');
EXCEPTION WHEN unique_violation THEN
  RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM;
END $$;
DO $$
DECLARE x int;
BEGIN
  BEGIN
    INSERT INTO t VALUES (100, 'rolled back');
    x := 1 / 0;
  EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
    RAISE NOTICE 'div: %', SQLERRM;
  END;
  RAISE NOTICE 'exists after rollback: %', EXISTS (SELECT 1 FROM t WHERE id = 100);
END $$;
DO $$
BEGIN
  RAISE EXCEPTION 'custom % thing', 7 USING ERRCODE = 'P0042', DETAIL = 'some detail', HINT = 'a hint';
EXCEPTION WHEN SQLSTATE 'P0042' THEN
  RAISE NOTICE 'caught custom: %', SQLERRM;
END $$;
DO $$
DECLARE msg text; st text; det text;
BEGIN
  PERFORM 1 / 0;
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS msg = MESSAGE_TEXT, st = RETURNED_SQLSTATE, det = PG_EXCEPTION_DETAIL;
  RAISE NOTICE 'diag % % [%]', st, msg, det;
END $$;
DO $$ BEGIN BEGIN PERFORM 1 / 0; EXCEPTION WHEN data_exception THEN RAISE NOTICE 'class match'; END; END $$;
DO $$ BEGIN BEGIN PERFORM 1 / 0; EXCEPTION WHEN unique_violation THEN RAISE NOTICE 'wrong'; END; END $$;
DO $$ BEGIN BEGIN RAISE EXCEPTION 'inner'; EXCEPTION WHEN OTHERS THEN RAISE; END; END $$;
DO $$ BEGIN RAISE EXCEPTION 'plain'; END $$;
DO $$ BEGIN RAISE division_by_zero; END $$;
DO $$ BEGIN RAISE SQLSTATE '22012'; END $$;
DO $$ BEGIN RAISE EXCEPTION USING MESSAGE = 'only using', ERRCODE = 'check_violation'; END $$;
DO $$ BEGIN ASSERT 1 = 1; ASSERT 1 = 2, 'one is not two'; END $$;
DO $$ BEGIN ASSERT false; END $$;

-- functions
CREATE FUNCTION add1(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a + 1; END $$;
SELECT add1(41), add1(NULL);
CREATE FUNCTION add_strict(a int) RETURNS int LANGUAGE plpgsql STRICT AS $$ BEGIN RAISE NOTICE 'called'; RETURN a + 1; END $$;
SELECT add_strict(NULL), add_strict(1);
CREATE FUNCTION cnt() RETURNS bigint LANGUAGE plpgsql AS $$ DECLARE n bigint; BEGIN SELECT count(*) INTO n FROM t; RETURN n; END $$;
SELECT cnt();
CREATE FUNCTION evens(n int) RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN FOR i IN 1..n LOOP IF i % 2 = 0 THEN RETURN NEXT i; END IF; END LOOP; END $$;
SELECT * FROM evens(7);
CREATE FUNCTION tab(lim int) RETURNS TABLE(id int, v text) LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT t.id, t.v FROM t WHERE t.id < lim ORDER BY t.id; END $$;
SELECT * FROM tab(4);
CREATE FUNCTION outs(a int, OUT b int, OUT c text) LANGUAGE plpgsql AS $$ BEGIN b := a * 2; c := 'x' || a; END $$;
SELECT * FROM outs(5);
SELECT (outs(6)).c;
CREATE FUNCTION fact(n int) RETURNS numeric LANGUAGE plpgsql AS $$ BEGIN IF n <= 1 THEN RETURN 1; END IF; RETURN n * fact(n - 1); END $$;
SELECT fact(20);
CREATE FUNCTION noret() RETURNS int LANGUAGE plpgsql AS $$ BEGIN END $$;
SELECT noret();
CREATE FUNCTION getrow(i int) RETURNS t LANGUAGE plpgsql AS $$ DECLARE r t; BEGIN SELECT * INTO r FROM t WHERE id = i; RETURN r; END $$;
SELECT * FROM getrow(2);
SELECT (getrow(3)).v;
CREATE FUNCTION defs(a int, b int DEFAULT 10) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a + b; END $$;
SELECT defs(1), defs(1, 2);
CREATE FUNCTION dollar(int, text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN $2 || $1; END $$;
SELECT dollar(1, 'a');
CREATE FUNCTION alias_fn(int) RETURNS int LANGUAGE plpgsql AS $$ DECLARE x ALIAS FOR $1; BEGIN RETURN x * 3; END $$;
SELECT alias_fn(4);
CREATE FUNCTION vfn() RETURNS void LANGUAGE plpgsql AS $$ BEGIN INSERT INTO t VALUES (50, 'from void'); END $$;
SELECT vfn();
SELECT v FROM t WHERE id = 50;
CREATE FUNCTION typed(x t.v%TYPE) RETURNS text LANGUAGE plpgsql AS $$ DECLARE y t.id%TYPE := 7; r t%ROWTYPE; BEGIN r.id := y; r.v := x; RETURN r.v || r.id; END $$;
SELECT typed('q');
CREATE FUNCTION arr() RETURNS int[] LANGUAGE plpgsql AS $$ DECLARE a int[] := '{}'; BEGIN a[1] := 10; a[3] := 30; RETURN a; END $$;
SELECT arr();
CREATE FUNCTION volatile_count() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO t VALUES ((SELECT max(id) + 1 FROM t), 'v'); RETURN 1; END $$;
SELECT volatile_count() FROM generate_series(1, 3);
SELECT count(*) FROM t WHERE v = 'v';
CREATE FUNCTION bad() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETRN 1; END $$;
CREATE FUNCTION bad2() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1 +; END $$;
CREATE FUNCTION bad3() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE '% %', 1; END $$;
CREATE FUNCTION bad4() RETURNS int LANGUAGE plpgsql AS $$ BEGIN IF true THEN RETURN 1; END; END $$;
CREATE FUNCTION bad5() RETURNS int LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  x := 1;
  RAISE NOTICE '%', 1, 2;
  RETURN x;
END $$;
DO $$
BEGIN
  PERFORM 1;

  RAISE NOTICE '% %',
    1;
END $$;
SET check_function_bodies = off;
CREATE FUNCTION bad6() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% %', 1;
  RETURN 1;
END $$;
SELECT bad6();
CREATE FUNCTION bad7() RETURNS int LANGUAGE sql AS 'SELEC 1';
SELECT bad7();
CREATE FUNCTION bad8() RETURNS int LANGUAGE sql AS 'SELECT ''x''::text';
SELECT bad8();
RESET check_function_bodies;
DROP FUNCTION bad6(), bad7(), bad8();
CREATE FUNCTION retvoid() RETURNS void LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION div0() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1 / 0; END $$;
SELECT div0();
DROP FUNCTION add1(int);
SELECT add1(1);

-- triggers
CREATE TABLE items (id serial PRIMARY KEY, name text, price numeric, updated_at timestamptz, version int DEFAULT 0);
CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.updated_at := '2024-01-01 00:00:00+00';
  NEW.version := COALESCE(OLD.version, 0) + 1;
  RETURN NEW;
END $$;
CREATE TRIGGER items_touch BEFORE INSERT OR UPDATE ON items FOR EACH ROW EXECUTE FUNCTION touch();
INSERT INTO items (name, price) VALUES ('a', 1), ('b', 2) RETURNING id, version, updated_at;
UPDATE items SET price = price * 2 WHERE id = 1 RETURNING id, price, version;
CREATE TABLE audit (n serial, op text, tbl text, old_name text, new_name text, lvl text, nargs int, args text[]);
CREATE FUNCTION log_it() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit (op, tbl, old_name, new_name, lvl, nargs, args)
    VALUES (TG_OP, TG_TABLE_NAME, CASE WHEN TG_OP <> 'INSERT' THEN OLD.name END, CASE WHEN TG_OP <> 'DELETE' THEN NEW.name END, TG_LEVEL, TG_NARGS, TG_ARGV);
  RETURN NULL;
END $$;
CREATE TRIGGER items_audit AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION log_it('x', 'y');
CREATE TRIGGER items_audit_stmt AFTER INSERT ON items FOR EACH STATEMENT EXECUTE PROCEDURE log_it();
INSERT INTO items (name, price) VALUES ('c', 3);
UPDATE items SET name = 'bb' WHERE id = 2;
DELETE FROM items WHERE id = 1;
SELECT op, tbl, old_name, new_name, lvl, nargs, args FROM audit ORDER BY n;
CREATE FUNCTION no_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.name = 'bb' THEN RETURN NULL; END IF; RETURN OLD; END $$;
CREATE TRIGGER items_guard BEFORE DELETE ON items FOR EACH ROW EXECUTE FUNCTION no_delete();
DELETE FROM items;
SELECT id, name, version FROM items ORDER BY id;
CREATE FUNCTION skip_neg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.price < 0 THEN RETURN NULL; END IF; RETURN NEW; END $$;
CREATE TRIGGER items_skip BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION skip_neg();
INSERT INTO items (name, price) VALUES ('neg', -1), ('pos', 1);
SELECT name FROM items ORDER BY id;
CREATE FUNCTION note() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE '% % % %', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP; RETURN NULL; END $$;
CREATE TRIGGER s1 BEFORE UPDATE ON items FOR EACH STATEMENT EXECUTE FUNCTION note();
CREATE TRIGGER s2 AFTER UPDATE ON items FOR EACH STATEMENT EXECUTE FUNCTION note();
UPDATE items SET price = 0 WHERE false;
CREATE TRIGGER w BEFORE UPDATE OF price ON items FOR EACH ROW WHEN (OLD.price IS DISTINCT FROM NEW.price) EXECUTE FUNCTION touch();
UPDATE items SET price = 5 WHERE name = 'pos';
UPDATE items SET name = 'pos2' WHERE name = 'pos';
SELECT tgname, tgtype, tgenabled, tgnargs FROM pg_trigger WHERE tgrelid = 'items'::regclass ORDER BY tgname;
ALTER TABLE items DISABLE TRIGGER s1;
UPDATE items SET price = 6 WHERE false;
ALTER TABLE items DISABLE TRIGGER ALL;
UPDATE items SET price = 6 WHERE false;
ALTER TABLE items ENABLE TRIGGER ALL;
ALTER TRIGGER s2 ON items RENAME TO s3;
UPDATE items SET price = 7 WHERE false;
DROP FUNCTION note();
DROP TRIGGER w ON items;
DROP TRIGGER IF EXISTS w ON items;
DROP TRIGGER w ON items;
CREATE TRIGGER items_touch BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION touch();
CREATE TRIGGER bad BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION cnt();
CREATE TRIGGER bad BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION nope();
CREATE TRIGGER bad BEFORE INSERT ON items FOR EACH ROW WHEN (OLD.id = 1) EXECUTE FUNCTION touch();
CREATE TRIGGER bad BEFORE TRUNCATE ON items FOR EACH ROW EXECUTE FUNCTION touch();
DROP TABLE items CASCADE;
SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'items%';
-- compile-time checks
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN EXIT; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN CONTINUE; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN EXIT foo; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ <<b>> BEGIN LOOP CONTINUE b; END LOOP; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ <<b>> BEGIN EXIT b; END $$;
DROP FUNCTION chk();
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN LOOP BEGIN EXIT; END; END LOOP; RETURN 1; END $$;
SELECT chk();
DROP FUNCTION chk();
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN NEXT 1; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 1; END $$;
CREATE FUNCTION chk(OUT a int) LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION chk() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION chk() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN NEXT; END $$;
CREATE FUNCTION chk(OUT a int) RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN NEXT 1; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1 $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN PERFORM 1 +; END $$;
CREATE FUNCTION chk() RETURNS int LANGUAGE plpgsql AS $$ BEGIN IF 1 + THEN RETURN 1; END IF; END $$;
DO $$ BEGIN RETRN 1; END $$;
DO $$ BEGIN RETURN 1; END $$;
DO $$ BEGIN IF true THEN NULL; END; END $$;
DO $$ BEGIN PERFORM 1 +; END $$;
CREATE FUNCTION chk(a t.nope%TYPE) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION chk(a nope.v%TYPE) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION chk(a t.v%TYPE) RETURNS t.id%TYPE LANGUAGE plpgsql AS $$ BEGIN RETURN length(a); END $$;
SELECT chk('abcd');
DROP FUNCTION chk(text);
