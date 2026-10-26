-- file_fdw: its validator (options by catalog, privileges, COPY's checks), the program
-- option where PostgreSQL accepts it (Django's inspectdb test), the commands a wrapper
-- without modify routines refuses, and EXPLAIN of a file that doesn't exist (no rows read).
SET client_min_messages = warning;
DROP ROLE IF EXISTS a66_fdw_user;
RESET client_min_messages;
CREATE EXTENSION file_fdw;
CREATE SERVER a66_srv FOREIGN DATA WRAPPER file_fdw;
CREATE FOREIGN TABLE a66_t (a int, b text) SERVER a66_srv
  OPTIONS (filename '/nonexistent/a66.csv', format 'csv', header 'match');
CREATE FOREIGN TABLE a66_x () SERVER a66_srv OPTIONS (filenam 'x');
CREATE FOREIGN TABLE a66_x () SERVER a66_srv OPTIONS (filename 'a', program 'b');
CREATE FOREIGN TABLE a66_x () SERVER a66_srv OPTIONS (filename 'a', on_error 'ignore', reject_limit 'x');
CREATE FOREIGN TABLE a66_x () SERVER a66_srv OPTIONS (filename 'a', on_error 'ignore', reject_limit '3');
DROP FOREIGN TABLE a66_x;
CREATE FOREIGN TABLE a66_x () SERVER a66_srv OPTIONS (filename 'a', header 'maybe');
CREATE FOREIGN TABLE a66_x () SERVER a66_srv OPTIONS ("a=b" 'true');
CREATE FOREIGN TABLE a66_x (c int OPTIONS (force_quote 'true')) SERVER a66_srv OPTIONS (filename 'a');
ALTER FOREIGN TABLE a66_t ALTER COLUMN a OPTIONS (force_not_null 'maybe');
ALTER FOREIGN TABLE a66_t ALTER COLUMN a OPTIONS (force_not_null 'on', force_null 'off');
ALTER FOREIGN TABLE a66_t OPTIONS (SET format 'text');
ALTER FOREIGN TABLE a66_t OPTIONS (DROP filename);
ALTER SERVER a66_srv OPTIONS (ADD filename 'x');
ALTER FOREIGN DATA WRAPPER file_fdw OPTIONS (ADD format 'csv');
-- (Django's inspectdb: a program the table never runs.)
CREATE FOREIGN TABLE a66_p (a real) SERVER a66_srv OPTIONS (program 'cat -n');
SELECT c.relname, ft.ftoptions, (SELECT array_agg(attfdwoptions ORDER BY attnum) FROM pg_attribute
  WHERE attrelid = c.oid AND attnum > 0) AS cols
FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid ORDER BY 1;
SELECT file_fdw_validator('{filename=x,foo=1}', 'pg_foreign_table'::regclass);
SELECT file_fdw_validator('{force_not_null=true,force_not_null=false}', 'pg_attribute'::regclass);
SELECT file_fdw_validator('{program=x}', 'pg_foreign_server'::regclass);
SELECT file_fdw_validator('{}', 'pg_user_mapping'::regclass);
SELECT file_fdw_validator(NULL, 0) IS NULL;
-- privileges: filename needs pg_read_server_files, program pg_execute_server_program
CREATE ROLE a66_fdw_user;
GRANT USAGE ON FOREIGN SERVER a66_srv TO a66_fdw_user;
GRANT CREATE ON SCHEMA public TO a66_fdw_user;
SET ROLE a66_fdw_user;
CREATE FOREIGN TABLE a66_u () SERVER a66_srv OPTIONS (filename 'a');
CREATE FOREIGN TABLE a66_u () SERVER a66_srv OPTIONS (program 'a');
RESET ROLE;
GRANT pg_read_server_files TO a66_fdw_user;
SET ROLE a66_fdw_user;
CREATE FOREIGN TABLE a66_u () SERVER a66_srv OPTIONS (filename 'a');
CREATE FOREIGN TABLE a66_v () SERVER a66_srv OPTIONS (program 'a');
SELECT * FROM a66_t;
DROP FOREIGN TABLE a66_u;
RESET ROLE;
-- no modify routines
INSERT INTO a66_t VALUES (1, 'x');
UPDATE a66_t SET a = 1;
DELETE FROM a66_t WHERE a = 1;
TRUNCATE a66_t;
INSERT INTO a66_t SELECT 1, 'x' WHERE false;
CREATE TABLE a66_parent (a int, b text);
ALTER FOREIGN TABLE a66_t INHERIT a66_parent;
UPDATE a66_parent SET a = 2;
DELETE FROM a66_parent;
SELECT * FROM ONLY a66_parent;
ALTER FOREIGN TABLE a66_t NO INHERIT a66_parent;
DROP TABLE a66_parent;
-- EXPLAIN of a file that isn't there: no size, the planner's default of ten pages
EXPLAIN (COSTS OFF) SELECT * FROM a66_t WHERE a > 1;
EXPLAIN SELECT * FROM a66_t;
EXPLAIN SELECT a FROM a66_t WHERE b = 'x';
EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM a66_p;
EXPLAIN (FORMAT JSON, COSTS OFF) SELECT * FROM a66_t;
DROP EXTENSION file_fdw CASCADE;
REVOKE CREATE ON SCHEMA public FROM a66_fdw_user;
DROP ROLE a66_fdw_user;
