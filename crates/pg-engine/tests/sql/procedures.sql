-- CALL
CREATE TABLE proc_t (id int, note text);
CREATE PROCEDURE ins(a int, b text DEFAULT 'dflt') LANGUAGE sql AS $$ INSERT INTO proc_t VALUES (a, b) $$;
CALL ins(1);
CALL ins(2, 'two');
CALL ins(b => 'named', a => 3);
CALL public.ins(4, 'qualified');
SELECT * FROM proc_t ORDER BY id;
CREATE PROCEDURE plp(INOUT x int, OUT y text) LANGUAGE plpgsql AS $$ BEGIN x := x * 2; y := 'doubled'; END $$;
CALL plp(21, NULL);
CALL plp(1);
CREATE PROCEDURE inout_sql(INOUT a int, INOUT b text) LANGUAGE sql AS $$ SELECT a + 1, b || '!' $$;
CALL inout_sql(1, 'x');
CREATE PROCEDURE noargs() LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'noargs ran'; END $$;
CALL noargs();
SELECT plp(1, NULL);
SELECT noargs();
CREATE FUNCTION fn1() RETURNS int LANGUAGE sql AS 'SELECT 1';
CALL fn1();
CALL nope();
CALL ins();
CALL ins('x');
CREATE FUNCTION nf(a int, b int DEFAULT 10) RETURNS int LANGUAGE sql AS 'SELECT a * 100 + b';
SELECT nf(b => 1, a => 2), nf(1), nf(a => 5), nf(3, b => 4);
SELECT nf(c => 1);
SELECT nf(b => 1);
DROP FUNCTION nf(int, int);
SELECT proname, prokind, prorettype::regtype FROM pg_proc WHERE proname IN ('ins', 'plp', 'fn1', 'noargs') ORDER BY 1;

-- transaction control
CREATE PROCEDURE tc() LANGUAGE plpgsql AS $$
BEGIN
  FOR i IN 1..4 LOOP
    INSERT INTO proc_t VALUES (10 + i, 'tc');
    IF i % 2 = 0 THEN COMMIT; ELSE ROLLBACK; END IF;
  END LOOP;
END $$;
CALL tc();
SELECT * FROM proc_t WHERE note = 'tc' ORDER BY id;
BEGIN;
CALL tc();
ROLLBACK;
DO $$
BEGIN
  INSERT INTO proc_t VALUES (20, 'do');
  COMMIT;
  INSERT INTO proc_t VALUES (21, 'do');
  ROLLBACK;
END $$;
SELECT * FROM proc_t WHERE note = 'do' ORDER BY id;
DO $$
BEGIN
  BEGIN
    COMMIT;
  EXCEPTION WHEN others THEN
    RAISE NOTICE 'caught: %', SQLERRM;
  END;
END $$;
DO $$
BEGIN
  INSERT INTO proc_t VALUES (22, 'sub');
  BEGIN
    INSERT INTO proc_t VALUES (23, 'sub');
  EXCEPTION WHEN others THEN NULL;
  END;
  COMMIT;
END $$;
SELECT * FROM proc_t WHERE note = 'sub' ORDER BY id;
CREATE FUNCTION fc() RETURNS void LANGUAGE plpgsql AS $$ BEGIN COMMIT; END $$;
SELECT fc();
CREATE PROCEDURE via_fn() LANGUAGE plpgsql AS $$ BEGIN PERFORM fc(); END $$;
CALL via_fn();
CREATE PROCEDURE sqlcommit() LANGUAGE sql AS $$ COMMIT $$;
CALL sqlcommit();
DO $$
BEGIN
  INSERT INTO proc_t VALUES (30, 'chain');
  COMMIT AND CHAIN;
  RAISE NOTICE 'chained %', current_setting('transaction_isolation');
  ROLLBACK AND CHAIN;
END $$;
SELECT * FROM proc_t WHERE note = 'chain' ORDER BY id;
DO $$
DECLARE r record;
BEGIN
  FOR r IN SELECT id FROM proc_t WHERE note = 'tc' ORDER BY id LOOP
    UPDATE proc_t SET note = 'tc-looped' WHERE id = r.id;
    COMMIT;
  END LOOP;
END $$;
SELECT * FROM proc_t WHERE note LIKE 'tc%' ORDER BY id;
DO $$
DECLARE c refcursor;
BEGIN
  OPEN c FOR SELECT 1;
  COMMIT;
  CLOSE c;
END $$;
DO $$
BEGIN
  PERFORM 1;
  COMMIT;
  RAISE NOTICE 'same xact time: %', transaction_timestamp() = statement_timestamp();
END $$;

-- CALL from PL/pgSQL
CREATE PROCEDURE outer_p() LANGUAGE plpgsql AS $$
DECLARE a int := 5; b text;
BEGIN
  CALL plp(a, b);
  RAISE NOTICE 'a=% b=%', a, b;
  CALL tc();
  CALL ins(40, 'nested');
END $$;
CALL outer_p();
SELECT count(*) FROM proc_t WHERE note IN ('tc', 'nested');
DO $$
DECLARE r int := 3;
BEGIN
  CALL plp(r, NULL);
END $$;
DO $$
BEGIN
  CALL plp(7, NULL);
END $$;
DO $$
DECLARE x int := 2; y text;
BEGIN
  CALL plp(x => x, y => y);
  RAISE NOTICE 'named x=% y=%', x, y;
END $$;

-- RETURN in procedures
CREATE PROCEDURE r1(x int) LANGUAGE plpgsql AS $$
BEGIN
  IF x > 0 THEN RETURN; END IF;
  RAISE NOTICE 'not returned';
END $$;
CALL r1(1);
CALL r1(0);
CREATE PROCEDURE r2() LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE PROCEDURE r3() RETURNS int LANGUAGE plpgsql AS $$ BEGIN END $$;

-- procedure DDL
ALTER PROCEDURE noargs() RENAME TO noargs2;
CALL noargs2();
DROP PROCEDURE plp(int);
DROP FUNCTION plp(int, text);
DROP PROCEDURE fn1();
DROP ROUTINE fn1();
DROP PROCEDURE plp(int, text);
DROP PROCEDURE IF EXISTS plp(int, text);
DROP PROCEDURE ins, inout_sql, noargs2, tc, via_fn, sqlcommit, outer_p, r1;
DROP FUNCTION fc();
DROP TABLE proc_t;
