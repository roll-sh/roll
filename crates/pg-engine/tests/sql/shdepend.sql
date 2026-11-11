-- Roles are cluster-wide in Postgres: clean up first and at the end.
DROP ROLE IF EXISTS gsd_r, gsd_s;
CREATE ROLE gsd_r;
CREATE ROLE gsd_s;
CREATE SCHEMA gsd_sch AUTHORIZATION gsd_r;
SET ROLE gsd_r;
CREATE TABLE gsd_sch.t (a int GENERATED ALWAYS AS IDENTITY, b serial, c text);
CREATE INDEX ON gsd_sch.t (c);
CREATE VIEW gsd_sch.v AS SELECT 1 AS x;
CREATE MATERIALIZED VIEW gsd_sch.mv AS SELECT 1 AS x;
CREATE SEQUENCE gsd_sch.s;
CREATE FUNCTION gsd_sch.f() RETURNS int LANGUAGE sql AS 'select 1';
CREATE TYPE gsd_sch.e AS ENUM ('a');
CREATE TYPE gsd_sch.c AS (a int);
CREATE DOMAIN gsd_sch.d AS int;
CREATE TYPE gsd_sch.rg AS RANGE (subtype = int4);
CREATE AGGREGATE gsd_sch.ag (int) (sfunc = int4pl, stype = int);
CREATE OPERATOR gsd_sch.### (leftarg = int, rightarg = int, function = int4pl);
CREATE TEXT SEARCH CONFIGURATION gsd_sch.cfg (COPY = english);
CREATE TEXT SEARCH DICTIONARY gsd_sch.dct (TEMPLATE = simple);
CREATE STATISTICS gsd_sch.st ON a, b FROM gsd_sch.t;
CREATE PROCEDURE gsd_sch.p() LANGUAGE sql AS 'select 1';
GRANT SELECT ON gsd_sch.t TO gsd_s;
GRANT SELECT (c) ON gsd_sch.t TO gsd_s;
GRANT USAGE ON SCHEMA gsd_sch TO gsd_s;
GRANT EXECUTE ON FUNCTION gsd_sch.f() TO gsd_s;
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO gsd_s;
ALTER DEFAULT PRIVILEGES IN SCHEMA gsd_sch GRANT SELECT ON TABLES TO gsd_s;
ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO gsd_s;
RESET ROLE;
CREATE TABLE gsd_pt (a int);
CREATE POLICY gsd_pol ON gsd_pt TO gsd_s, gsd_r, PUBLIC USING (true);
GRANT SELECT ON gsd_pt TO gsd_r WITH GRANT OPTION;
SET ROLE gsd_r;
GRANT SELECT ON gsd_pt TO gsd_s;
RESET ROLE;
SELECT lo_create(71001);
GRANT SELECT ON LARGE OBJECT 71001 TO gsd_s;
ALTER LARGE OBJECT 71001 OWNER TO gsd_r;
SELECT dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) AS thisdb, dbid = 0 AS shared, classid::regclass,
       CASE WHEN classid = 'pg_class'::regclass THEN objid::regclass::text WHEN classid = 'pg_type'::regclass THEN objid::regtype::text
            WHEN classid = 'pg_proc'::regclass THEN objid::regprocedure::text ELSE pg_describe_object(classid, objid, objsubid) END AS obj,
       objsubid, refclassid::regclass, refobjid::regrole, deptype
FROM pg_shdepend WHERE refobjid IN ('gsd_r'::regrole, 'gsd_s'::regrole)
ORDER BY refobjid::regrole::text, classid::regclass::text, obj, objsubid, deptype;
SELECT count(*) FROM pg_shdepend WHERE refobjid < 16384;
DROP ROLE gsd_r;
DROP ROLE gsd_s;
-- Built-in functions whose EXECUTE was revoked from PUBLIC.
SELECT p.oid::regprocedure, proacl FROM pg_proc p WHERE proacl IS NOT NULL AND p.oid < 16384 AND proname ~ '^(lo_|pg_read_file|pg_ls_dir|pg_reload_conf|pg_current_logfile|pg_stat_reset$)' ORDER BY p.oid;
SELECT has_function_privilege('gsd_s', 'pg_read_file(text)', 'EXECUTE'), has_function_privilege('gsd_s', 'pg_reload_conf()', 'EXECUTE'), has_function_privilege('gsd_s', 'now()', 'EXECUTE');
SELECT has_function_privilege('pg_monitor', 'pg_current_logfile()', 'EXECUTE'), has_function_privilege('pg_monitor', 'pg_reload_conf()', 'EXECUTE');
SET ROLE gsd_s;
SELECT pg_reload_conf();
SELECT * FROM pg_ls_dir('.');
RESET ROLE;
GRANT EXECUTE ON FUNCTION pg_reload_conf() TO gsd_s;
GRANT EXECUTE ON FUNCTION pg_catalog.pg_read_file(text, bigint, bigint) TO gsd_s;
GRANT EXECUTE ON FUNCTION pg_read_file(int) TO gsd_s;
REVOKE EXECUTE ON FUNCTION abs(int) FROM PUBLIC;
SELECT p.oid::regprocedure, proacl FROM pg_proc p WHERE p.oid IN ('pg_reload_conf()'::regprocedure, 'pg_read_file(text,bigint,bigint)'::regprocedure, 'abs(int)'::regprocedure) ORDER BY p.oid;
SELECT has_function_privilege('gsd_s', 'pg_reload_conf()', 'EXECUTE'), has_function_privilege('gsd_s', 'abs(int)', 'EXECUTE'), has_function_privilege('gsd_s', 'abs(bigint)', 'EXECUTE');
SET ROLE gsd_s;
SELECT abs(-1);
SELECT abs(-1::bigint);
RESET ROLE;
DROP ROLE gsd_s;
GRANT EXECUTE ON FUNCTION abs(int) TO PUBLIC;
REVOKE EXECUTE ON FUNCTION pg_reload_conf() FROM gsd_s;
REVOKE EXECUTE ON FUNCTION pg_read_file(text, bigint, bigint) FROM gsd_s;
SELECT p.oid::regprocedure, proacl FROM pg_proc p WHERE p.oid IN ('pg_reload_conf()'::regprocedure, 'pg_read_file(text,bigint,bigint)'::regprocedure, 'abs(int)'::regprocedure) ORDER BY p.oid;
-- Cleanup.
SELECT lo_unlink(71001);
DROP POLICY gsd_pol ON gsd_pt;
DROP TABLE gsd_pt;
DROP OWNED BY gsd_r;
DROP OWNED BY gsd_s;
DROP ROLE gsd_r, gsd_s;
