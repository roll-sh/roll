-- A routine's statements resolve names (reg* input, relations, functions, types) through the
-- catalog as DDL an earlier statement ran (EXECUTE'd RENAME / CREATE) left it, as plancache
-- invalidates cached plans on DDL.
DROP ROLE IF EXISTS regress_a53_ren;
CREATE TABLE ren_a (x int);
CREATE FUNCTION ren_f() RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE TYPE ren_t AS (a int);
DO $$
DECLARE r regclass; p regproc; t regtype; n int;
BEGIN
  EXECUTE 'ALTER TABLE ren_a RENAME TO ren_b';
  r := 'ren_b'::regclass;
  RAISE NOTICE 'regclass %', r;
  SELECT count(*) INTO n FROM ren_b;
  RAISE NOTICE 'count %', n;
  EXECUTE 'ALTER FUNCTION ren_f() RENAME TO ren_g';
  p := 'ren_g'::regproc;
  RAISE NOTICE 'regproc % %', p, ren_g();
  EXECUTE 'ALTER TYPE ren_t RENAME TO ren_u';
  t := 'ren_u'::regtype;
  RAISE NOTICE 'regtype %', t;
  EXECUTE 'CREATE TABLE ren_c (y int)';
  INSERT INTO ren_c VALUES (5);
  RAISE NOTICE 'c % %', 'ren_c'::regclass, (SELECT y FROM ren_c);
  EXECUTE 'ALTER TABLE ren_c ADD COLUMN z int DEFAULT 7';
  RAISE NOTICE 'z %', (SELECT z FROM ren_c);
  EXECUTE 'CREATE SCHEMA ren_s';
  EXECUTE 'CREATE TABLE ren_s.q (v int)';
  RAISE NOTICE 'q %', 'ren_s.q'::regclass;
END $$;
DO $$
BEGIN
  FOR i IN 1..2 LOOP
    RAISE NOTICE 'loop % %', i, (SELECT count(*) FROM ren_b);
    IF i = 1 THEN EXECUTE 'ALTER TABLE ren_b RENAME TO ren_old'; EXECUTE 'CREATE TABLE ren_b AS SELECT 1 AS x UNION ALL SELECT 2'; END IF;
  END LOOP;
END $$;
CREATE FUNCTION ren_h() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE ren_d (w int)';
  RETURN 'ren_d'::regclass::text;
END $$;
SELECT ren_h();
SET check_function_bodies = off;
CREATE FUNCTION ren_sql() RETURNS text LANGUAGE sql AS $$
  CREATE TABLE ren_e (w int);
  ALTER TABLE ren_e RENAME TO ren_f;
  SELECT 'ren_f'::regclass::text || (SELECT count(*) FROM ren_f)
$$;
SELECT ren_sql();
CREATE PROCEDURE ren_p() LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE ren_g (w int)';
  COMMIT;
  EXECUTE 'ALTER TABLE ren_g RENAME TO ren_h';
  RAISE NOTICE '%', 'ren_h'::regclass;
END $$;
CALL ren_p();
DO $$
BEGIN
  EXECUTE 'CREATE FUNCTION ren_k() RETURNS int LANGUAGE sql AS ''SELECT 42''';
  RAISE NOTICE '% %', 'ren_k'::regproc, ren_k();
  EXECUTE 'CREATE TYPE ren_m AS ENUM (''a'', ''b'')';
  RAISE NOTICE '% %', 'ren_m'::regtype, 'b'::ren_m;
  EXECUTE 'CREATE ROLE regress_a53_ren';
  RAISE NOTICE '%', 'regress_a53_ren'::regrole;
  EXECUTE 'DROP ROLE regress_a53_ren';
END $$;
RESET check_function_bodies;
DROP ROLE IF EXISTS regress_a53_ren;
