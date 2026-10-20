-- Utility statements in SQL-language functions and procedures run statement by statement.
CREATE FUNCTION sfu_make() RETURNS void LANGUAGE sql AS $$ CREATE TABLE sfu_t (a int) $$;
SELECT sfu_make();
SELECT attname FROM pg_attribute WHERE attrelid = 'sfu_t'::regclass AND attnum > 0;
SELECT sfu_make();
CREATE FUNCTION sfu_alter() RETURNS void LANGUAGE sql AS $$ CREATE INDEX ON sfu_t (a); ALTER TABLE sfu_t ADD COLUMN b int; $$;
SELECT sfu_alter();
SELECT attname FROM pg_attribute WHERE attrelid = 'sfu_t'::regclass AND attnum > 0 ORDER BY attnum;
SELECT indexrelid::regclass FROM pg_index WHERE indrelid = 'sfu_t'::regclass;
CREATE FUNCTION sfu_mixed() RETURNS int LANGUAGE sql AS $$ INSERT INTO sfu_t VALUES (1, 2); COMMENT ON TABLE sfu_t IS 'made'; SELECT count(*)::int FROM sfu_t $$;
SELECT sfu_mixed();
SELECT obj_description('sfu_t'::regclass);
-- Analysis stops at nothing: a later statement's relation must exist when created.
CREATE FUNCTION sfu_later() RETURNS int LANGUAGE sql AS $$ CREATE TABLE sfu_t2 (a int); INSERT INTO sfu_t2 VALUES (1); SELECT 1 $$;
-- A utility statement isn't a result.
CREATE FUNCTION sfu_ret() RETURNS int LANGUAGE sql AS $$ CREATE TABLE sfu_t3 (a int) $$;
-- Statements that can't run in a function.
CREATE FUNCTION sfu_vac() RETURNS void LANGUAGE sql AS $$ VACUUM $$;
SELECT sfu_vac();
CREATE FUNCTION sfu_analyze() RETURNS void LANGUAGE sql AS $$ ANALYZE sfu_t $$;
SELECT sfu_analyze();
CREATE FUNCTION sfu_tx() RETURNS void LANGUAGE sql AS $$ COMMIT $$;
SELECT sfu_tx();
-- BEGIN ATOMIC bodies take only plannable statements.
CREATE FUNCTION sfu_atomic() RETURNS void LANGUAGE sql BEGIN ATOMIC CREATE TABLE sfu_x (a int); END;
CREATE FUNCTION sfu_atomic2() RETURNS void LANGUAGE sql BEGIN ATOMIC SELECT 1; NOTIFY sfu; END;
-- Procedures.
CREATE PROCEDURE sfu_proc(n text) LANGUAGE sql AS $$ CREATE TABLE sfu_p (a int) $$;
CALL sfu_proc('x');
SELECT count(*) FROM pg_class WHERE relname = 'sfu_p';
CREATE PROCEDURE sfu_proc2() LANGUAGE sql AS $$ DROP TABLE sfu_p; CREATE TABLE sfu_p2 (b text) $$;
CALL sfu_proc2();
SELECT relname FROM pg_class WHERE relname LIKE 'sfu_p%' AND relkind = 'r' ORDER BY 1;
