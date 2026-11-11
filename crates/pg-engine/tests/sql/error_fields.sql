-- Error object fields (SCHEMA / TABLE / COLUMN / DATATYPE / CONSTRAINT NAME): constraint
-- violations, RAISE ... USING, and GET STACKED DIAGNOSTICS.
CREATE TABLE ef_p (id int PRIMARY KEY, n int NOT NULL, c int CONSTRAINT ef_c_pos CHECK (c > 0));
CREATE TABLE ef_c (id int REFERENCES ef_p);
CREATE DOMAIN ef_d AS int NOT NULL CONSTRAINT ef_d_pos CHECK (VALUE > 0);
INSERT INTO ef_p VALUES (1, 1, 1);
INSERT INTO ef_p VALUES (1, 1, 1);
INSERT INTO ef_p VALUES (2, NULL, 1);
INSERT INTO ef_p VALUES (3, 1, 0);
INSERT INTO ef_c VALUES (7);
SELECT NULL::ef_d;
SELECT 0::ef_d;
CREATE FUNCTION ef_try(sql text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE s text; t text; c text; d text; n text; st text;
BEGIN
  EXECUTE sql;
  RETURN 'ok';
EXCEPTION WHEN others THEN
  GET STACKED DIAGNOSTICS st = RETURNED_SQLSTATE, s = SCHEMA_NAME, t = TABLE_NAME, c = COLUMN_NAME, d = PG_DATATYPE_NAME, n = CONSTRAINT_NAME;
  RETURN concat_ws('|', st, s, t, c, d, n);
END $$;
SELECT ef_try('INSERT INTO ef_p VALUES (1, 1, 1)');
SELECT ef_try('INSERT INTO ef_p VALUES (2, NULL, 1)');
SELECT ef_try('INSERT INTO ef_c VALUES (9)');
SELECT ef_try('SELECT 0::ef_d');
SELECT ef_try('SELECT 1/0');
DO $$ BEGIN RAISE EXCEPTION 'custom' USING COLUMN = 'col', CONSTRAINT = 'con', DATATYPE = 'typ', TABLE = 'tab', SCHEMA = 'sch'; END $$;
DO $$ BEGIN RAISE NOTICE 'note' USING TABLE = 'tab'; END $$;
DO $$ BEGIN RAISE EXCEPTION 'twice' USING DETAIL = 'a', DETAIL = 'b'; END $$;
DO $$ BEGIN RAISE EXCEPTION 'msg' USING MESSAGE = 'other'; END $$;
DO $$ BEGIN RAISE division_by_zero USING ERRCODE = '22000'; END $$;
DO $$ BEGIN RAISE division_by_zero USING MESSAGE = 'custom message'; END $$;
DO $$ DECLARE t text; BEGIN
  BEGIN RAISE EXCEPTION 'x' USING TABLE = 'tt', COLUMN = 'cc';
  EXCEPTION WHEN others THEN GET STACKED DIAGNOSTICS t = TABLE_NAME; RAISE NOTICE 'table %', t; RAISE;
  END;
END $$;
DROP FUNCTION ef_try(text);
DROP TABLE ef_c, ef_p;
DROP DOMAIN ef_d;
