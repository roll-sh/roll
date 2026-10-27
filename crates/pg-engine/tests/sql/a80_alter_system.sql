-- ALTER SYSTEM, postgresql.auto.conf as pg_file_settings shows it, pg_reload_conf, and
-- pg_settings' source, sourcefile, sourceline and pending_restart (agent 80).
ALTER SYSTEM RESET ALL;
SELECT pg_reload_conf();
SELECT pg_sleep(0.3);
DROP ROLE IF EXISTS regress_a80_u;
DROP ROLE IF EXISTS regress_a80_r;
SELECT sourcefile, sourceline, seqno, name, setting, applied, error FROM pg_file_settings ORDER BY seqno;
-- What ALTER SYSTEM refuses.
ALTER SYSTEM SET bogus = 1;
ALTER SYSTEM SET "bad name" = 1;
ALTER SYSTEM SET "a.b c" = 1;
ALTER SYSTEM RESET nonexist;
ALTER SYSTEM SET work_mem = 'abc';
ALTER SYSTEM SET log_min_duration_statement = 5000000000;
ALTER SYSTEM SET log_min_duration_statement = -5;
ALTER SYSTEM SET datestyle = 'bogus';
ALTER SYSTEM SET work_mem = E'1\nMB';
ALTER SYSTEM SET transaction_isolation = 'serializable';
ALTER SYSTEM SET transaction_read_only = on;
ALTER SYSTEM SET session_authorization = 'postgres';
ALTER SYSTEM SET role = 'none';
ALTER SYSTEM SET seed = 0.5;
ALTER SYSTEM SET server_version = '1';
ALTER SYSTEM SET data_directory = '/x';
ALTER SYSTEM SET config_file = '/x';
ALTER SYSTEM SET allow_alter_system = on;
BEGIN;
ALTER SYSTEM SET work_mem = '1MB';
ROLLBACK;
DO $$ BEGIN EXECUTE 'ALTER SYSTEM SET work_mem = ''1MB'''; END $$;
-- Privileges.
CREATE ROLE regress_a80_u;
CREATE ROLE regress_a80_r IN ROLE pg_read_all_settings;
SET ROLE regress_a80_u;
ALTER SYSTEM SET work_mem = '1MB';
ALTER SYSTEM RESET ALL;
ALTER SYSTEM RESET work_mem;
SELECT pg_reload_conf();
SELECT count(*) FROM pg_file_settings;
RESET ROLE;
GRANT ALTER SYSTEM ON PARAMETER work_mem TO regress_a80_u;
SET ROLE regress_a80_u;
ALTER SYSTEM SET work_mem = '1MB';
ALTER SYSTEM SET maintenance_work_mem = '100MB';
RESET ROLE;
-- What it writes.
ALTER SYSTEM SET work_mem = '8MB';
ALTER SYSTEM SET search_path = a, "B c", 'd';
ALTER SYSTEM SET "foo.Bar" = 'it''s \ x';
ALTER SYSTEM SET plpgsql.bogus = 1;
ALTER SYSTEM RESET plpgsql.bogus;
ALTER SYSTEM RESET "foo.nonexist";
ALTER SYSTEM SET shared_buffers = 128;
ALTER SYSTEM SET max_connections = 101;
ALTER SYSTEM SET statement_timeout TO 1000;
ALTER SYSTEM SET log_checkpoints = off;
ALTER SYSTEM SET "DateStyle" = 'sql, dmy';
ALTER SYSTEM SET work_mem = DEFAULT;
ALTER SYSTEM SET work_mem = '6MB';
SELECT sourcefile, sourceline, seqno, name, setting, applied, error FROM pg_file_settings ORDER BY seqno;
-- (Not applied yet; the file was read, so shared_buffers is pending a restart here.)
SHOW statement_timeout;
SELECT name, setting, source, sourcefile, sourceline, pending_restart FROM pg_settings
  WHERE name IN ('work_mem', 'shared_buffers', 'statement_timeout', 'max_connections') ORDER BY name;
SELECT pg_reload_conf();
SELECT pg_sleep(0.5);
SELECT pg_conf_load_time() > pg_postmaster_start_time();
SHOW statement_timeout;
SHOW work_mem;
SHOW search_path;
SHOW "foo.bar";
SHOW DateStyle;
SHOW log_checkpoints;
SHOW shared_buffers;
SELECT name, setting, source, sourcefile, sourceline, pending_restart FROM pg_settings
  WHERE name IN ('work_mem', 'shared_buffers', 'statement_timeout', 'max_connections', 'DateStyle',
                 'log_checkpoints', 'search_path', 'TimeZone') ORDER BY name;
SELECT name, reset_val FROM pg_settings WHERE name IN ('work_mem', 'statement_timeout') ORDER BY name;
-- A session's SET wins over the file, and RESET goes back to it.
SET work_mem = '2MB';
SELECT source, sourcefile FROM pg_settings WHERE name = 'work_mem';
RESET work_mem;
SHOW work_mem;
SET statement_timeout = 0;
-- Who sees where values come from.
SET ROLE regress_a80_u;
SELECT name, source, sourcefile, sourceline FROM pg_settings WHERE name = 'work_mem';
RESET ROLE;
SET ROLE regress_a80_r;
SELECT name, source, sourcefile, sourceline FROM pg_settings WHERE name = 'work_mem';
SELECT count(*) > 0 FROM pg_file_settings;
RESET ROLE;
-- Back to postgresql.conf's values.
ALTER SYSTEM RESET ALL;
SELECT sourcefile, sourceline, seqno, name, setting, applied, error FROM pg_file_settings ORDER BY seqno;
SELECT pg_reload_conf();
SELECT pg_sleep(0.5);
RESET statement_timeout;
SHOW statement_timeout;
SHOW work_mem;
SHOW search_path;
SHOW DateStyle;
SELECT current_setting('foo.bar', true);
SELECT name, setting, source, sourcefile, sourceline FROM pg_settings
  WHERE name IN ('work_mem', 'DateStyle', 'shared_buffers') ORDER BY name;
-- Contexts.
SET log_checkpoints = on;
SET shared_buffers = '1MB';
SET data_directory = '/x';
SET server_version = '1';
SET log_connections = on;
SELECT set_config('log_checkpoints', 'on', false);
SELECT set_config('work_mem', '1MB', true), current_setting('work_mem');
SELECT current_setting('nonexist', true) IS NULL, current_setting('foo.nonexist', true) IS NULL;
SELECT current_setting('foo.nonexist');
SELECT set_config('foo.x', 'y', false), current_setting('foo.x');
SELECT set_config('work_mem', NULL, false);
SELECT set_config(NULL, 'x', false);
SELECT set_config('work_mem', '2MB', NULL);
SELECT current_setting(NULL), current_setting('work_mem', NULL);
RESET work_mem;
REVOKE ALTER SYSTEM ON PARAMETER work_mem FROM regress_a80_u;
DROP ROLE regress_a80_u;
DROP ROLE regress_a80_r;
