-- PL/pgSQL variable / column name conflicts
CREATE TABLE vc_t (id int, val text);
INSERT INTO vc_t VALUES (1, 'one'), (2, 'two');
CREATE FUNCTION vc_err(id int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN (SELECT val FROM vc_t WHERE vc_t.id = id);
END $$;
SELECT vc_err(1);
CREATE FUNCTION vc_qualified(id int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN (SELECT val FROM vc_t WHERE vc_t.id = vc_qualified.id);
END $$;
SELECT vc_qualified(2);
CREATE FUNCTION vc_var(id int) RETURNS text LANGUAGE plpgsql AS $$
#variable_conflict use_variable
BEGIN
  RETURN (SELECT val FROM vc_t WHERE vc_t.id = id);
END $$;
SELECT vc_var(1);
CREATE FUNCTION vc_col(id int) RETURNS bigint LANGUAGE plpgsql AS $$
#variable_conflict use_column
BEGIN
  RETURN (SELECT count(*) FROM vc_t WHERE vc_t.id = id);
END $$;
SELECT vc_col(1);
CREATE FUNCTION vc_explicit_error(id int) RETURNS text LANGUAGE plpgsql AS $$
#variable_conflict error
DECLARE v text;
BEGIN
  SELECT val INTO v FROM vc_t WHERE id = 1;
  RETURN v;
END $$;
SELECT vc_explicit_error(1);
-- the setting
SET plpgsql.variable_conflict = use_variable;
CREATE FUNCTION vc_guc(id int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  RETURN (SELECT val FROM vc_t WHERE vc_t.id = id);
END $$;
SELECT vc_guc(2);
SET plpgsql.variable_conflict = bogus;
RESET plpgsql.variable_conflict;
SELECT vc_guc(2);
-- DO blocks, UPDATE / DELETE, record fields and labels
DO $$
DECLARE val text := 'x';
BEGIN
  UPDATE vc_t SET val = 'y' WHERE val = 'one';
END $$;
DO $$
DECLARE v text := 'x';
BEGIN
  UPDATE vc_t SET val = v WHERE id = 99;
  RAISE NOTICE 'no conflict';
END $$;
DO $$
<<blk>>
DECLARE id int := 2;
BEGIN
  PERFORM 1 FROM vc_t WHERE vc_t.id = blk.id;
  RAISE NOTICE 'label ok %', FOUND;
END $$;
DO $$
DECLARE vc_t record;
BEGIN
  SELECT 5 AS id INTO vc_t;
  PERFORM 1 FROM vc_t WHERE vc_t.id = 1;
END $$;
DO $$
DECLARE r record;
BEGIN
  FOR r IN SELECT id FROM vc_t ORDER BY id LOOP
    RAISE NOTICE 'id %', r.id;
  END LOOP;
END $$;
DO $$
DECLARE id int := 1;
BEGIN
  PERFORM (SELECT max(x.id) FROM (SELECT vc_t.id FROM vc_t) x);
  RAISE NOTICE 'subquery ok';
  PERFORM (SELECT max(id) FROM (SELECT vc_t.id FROM vc_t) x);
END $$;
DO $$
DECLARE id int := 1;
BEGIN
  INSERT INTO vc_t (id, val) VALUES (id + 10, 'ins');
  RAISE NOTICE 'insert ok';
  INSERT INTO vc_t SELECT id + 20, 'sel' FROM vc_t;
END $$;
SELECT * FROM vc_t ORDER BY id;
-- SQL functions: columns win
CREATE FUNCTION vc_sql(id int) RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM vc_t WHERE vc_t.id = id';
SELECT vc_sql(1);
DROP FUNCTION vc_err(int), vc_qualified(int), vc_var(int), vc_col(int), vc_explicit_error(int), vc_guc(int), vc_sql(int);
DROP TABLE vc_t;
