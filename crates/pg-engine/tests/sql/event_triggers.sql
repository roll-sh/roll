-- event trigger functions
CREATE FUNCTION et_fn() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'et_fn: % %', tg_event, tg_tag;
END $$;
SELECT et_fn();
CREATE FUNCTION et_arg(x int) RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
CREATE FUNCTION et_sql() RETURNS event_trigger LANGUAGE sql AS $$ SELECT 1 $$;
CREATE FUNCTION tg_arg(x int) RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
-- CREATE EVENT TRIGGER and its errors
CREATE EVENT TRIGGER et_bad ON ddl_command_start EXECUTE FUNCTION pg_backend_pid();
CREATE EVENT TRIGGER et_bad ON elephant_bootstrap EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON ddl_command_start WHEN food IN ('x') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON ddl_command_start WHEN tag IN ('create skunkcabbage') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON ddl_command_start WHEN tag IN ('CREATE ROLE') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON ddl_command_start WHEN tag IN ('drop event trigger') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON ddl_command_start WHEN tag IN ('create table') AND tag IN ('create view') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON table_rewrite WHEN tag IN ('create table') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON login WHEN tag IN ('create table') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_bad ON ddl_command_start EXECUTE FUNCTION no_such_fn();
CREATE EVENT TRIGGER et_start ON ddl_command_start EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_end ON ddl_command_end EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_tab ON ddl_command_start WHEN TAG IN ('create table', 'CREATE FUNCTION') EXECUTE FUNCTION et_fn();
CREATE EVENT TRIGGER et_start ON ddl_command_start EXECUTE FUNCTION et_fn();
SELECT evtname, evtevent, evtenabled, evttags FROM pg_event_trigger ORDER BY 1;
-- firing, enabling and disabling
CREATE TABLE et_t1 (a int);
CREATE VIEW et_v1 AS SELECT 1 AS x;
ALTER EVENT TRIGGER et_start DISABLE;
CREATE TABLE et_t2 (a int);
SET session_replication_role = replica;
CREATE TABLE et_t3 (a int);
ALTER EVENT TRIGGER et_start ENABLE REPLICA;
CREATE TABLE et_t4 (a int);
ALTER EVENT TRIGGER et_start ENABLE ALWAYS;
CREATE TABLE et_t5 (a int);
RESET session_replication_role;
CREATE TABLE et_t6 (a int);
CREATE FUNCTION et_inner() RETURNS int LANGUAGE plpgsql AS $$ BEGIN CREATE TABLE et_t7 (a int); RETURN 0; END $$;
SELECT et_inner();
-- not for global objects or event triggers themselves
CREATE ROLE et_role;
DROP ROLE et_role;
ALTER EVENT TRIGGER et_start RENAME TO et_tab;
ALTER EVENT TRIGGER et_start RENAME TO et_begin;
ALTER EVENT TRIGGER et_nope DISABLE;
COMMENT ON EVENT TRIGGER et_begin IS 'starts';
SELECT obj_description(oid, 'pg_event_trigger') FROM pg_event_trigger WHERE evtname = 'et_begin';
SELECT pg_describe_object('pg_event_trigger'::regclass, oid, 0), (pg_identify_object('pg_event_trigger'::regclass, oid, 0)).* FROM pg_event_trigger WHERE evtname = 'et_begin';
DROP FUNCTION et_fn();
DROP EVENT TRIGGER et_begin;
DROP EVENT TRIGGER et_end;
DROP EVENT TRIGGER et_tab;
DROP EVENT TRIGGER et_tab;
DROP EVENT TRIGGER IF EXISTS et_tab;
DROP TABLE et_t1, et_t2, et_t3, et_t4, et_t5, et_t6, et_t7;
DROP VIEW et_v1;
-- what sql_drop and ddl_command_end see
CREATE FUNCTION et_dropped() RETURNS event_trigger LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  FOR r IN SELECT * FROM pg_event_trigger_dropped_objects() LOOP
    -- (oids in generated names masked)
    RAISE NOTICE 'DROP: % orig=% normal=% temp=% type=% identity=% schema=% name=% addr=% args=%',
      tg_tag, r.original, r.normal, r.is_temporary, r.object_type,
      regexp_replace(r.object_identity, '[0-9]{4,}', 'N', 'g'), r.schema_name,
      regexp_replace(r.object_name, '[0-9]{4,}', 'N', 'g'),
      regexp_replace(r.address_names::text, '[0-9]{4,}', 'N', 'g'), r.address_args;
  END LOOP;
END $$;
CREATE FUNCTION et_cmds() RETURNS event_trigger LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  FOR r IN SELECT * FROM pg_event_trigger_ddl_commands() LOOP
    RAISE NOTICE 'END: % type=% identity=% schema=%', r.command_tag, r.object_type, r.object_identity, r.schema_name;
  END LOOP;
END $$;
CREATE EVENT TRIGGER et_drop ON sql_drop EXECUTE FUNCTION et_dropped();
CREATE EVENT TRIGGER et_cmd ON ddl_command_end EXECUTE FUNCTION et_cmds();
CREATE SCHEMA et_s;
CREATE TABLE et_s.one (id serial PRIMARY KEY, b text DEFAULT 'x');
CREATE INDEX et_one_b ON et_s.one (b);
CREATE TABLE et_s.two (c int CHECK (c > 0) REFERENCES et_s.one);
ALTER TABLE et_s.two DROP COLUMN c;
ALTER TABLE et_s.one ALTER COLUMN b DROP DEFAULT;
CREATE OR REPLACE FUNCTION et_s.f(int) RETURNS int LANGUAGE sql AS 'SELECT $1';
CREATE TRIGGER et_trig BEFORE INSERT ON et_s.one FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger();
CREATE POLICY et_pol ON et_s.one USING (true);
DROP INDEX et_s.et_one_b;
DROP SCHEMA et_s CASCADE;
CREATE TEMP TABLE et_tmp (a int PRIMARY KEY);
DROP TABLE et_tmp;
SELECT * FROM pg_event_trigger_dropped_objects();
SELECT * FROM pg_event_trigger_ddl_commands();
SELECT pg_event_trigger_table_rewrite_oid();
DROP EVENT TRIGGER et_drop;
DROP EVENT TRIGGER et_cmd;
-- table rewrites
CREATE FUNCTION et_rw() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'rewriting % (reason %) for %', pg_event_trigger_table_rewrite_oid()::regclass, pg_event_trigger_table_rewrite_reason(), tg_tag;
END $$;
CREATE EVENT TRIGGER et_rewrite ON table_rewrite EXECUTE FUNCTION et_rw();
CREATE TABLE et_rw_t (id int, f float, ts timestamptz);
INSERT INTO et_rw_t VALUES (1, 1.5, now());
ALTER TABLE et_rw_t ALTER COLUMN f TYPE numeric;
ALTER TABLE et_rw_t ADD COLUMN g int DEFAULT 0;
ALTER TABLE et_rw_t ADD COLUMN h float DEFAULT random();
ALTER TABLE et_rw_t ADD COLUMN i int DEFAULT 1, ALTER COLUMN f TYPE numeric(10,4);
BEGIN;
SET LOCAL timezone = 'UTC';
ALTER TABLE et_rw_t ALTER COLUMN ts TYPE timestamp;
SET LOCAL timezone = 'Europe/London';
ALTER TABLE et_rw_t ALTER COLUMN ts TYPE timestamptz;
ROLLBACK;
DROP EVENT TRIGGER et_rewrite;
DROP TABLE et_rw_t;
SET event_triggers = off;
SHOW event_triggers;
RESET event_triggers;
