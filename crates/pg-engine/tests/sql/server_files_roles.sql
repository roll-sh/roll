-- Server-side file access: the predefined roles and the checks made before any file is
-- touched (the same whether the server allows server files or not). Roles are cluster-wide:
-- clean up first and at the end.
DROP ROLE IF EXISTS gsf_user, gsf_reader, gsf_noinherit;
SELECT oid, rolname, rolsuper, rolinherit, rolcanlogin FROM pg_roles
WHERE rolname IN ('pg_read_server_files', 'pg_write_server_files', 'pg_execute_server_program') ORDER BY oid;
SELECT count(*) FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid
WHERE r.rolname IN ('pg_read_server_files', 'pg_write_server_files', 'pg_execute_server_program');
CREATE ROLE gsf_user;
CREATE ROLE gsf_reader;
CREATE ROLE gsf_noinherit NOINHERIT;
GRANT pg_read_server_files TO gsf_reader;
GRANT pg_read_server_files TO gsf_noinherit;
SELECT pg_has_role('gsf_reader', 'pg_read_server_files', 'USAGE'),
       pg_has_role('gsf_noinherit', 'pg_read_server_files', 'USAGE'),
       pg_has_role('gsf_noinherit', 'pg_read_server_files', 'MEMBER'),
       pg_has_role('gsf_user', 'pg_write_server_files', 'MEMBER');
SELECT f, has_function_privilege('gsf_reader', f, 'EXECUTE'), has_function_privilege('postgres', f, 'EXECUTE')
FROM unnest(array['pg_read_file(text)', 'pg_read_binary_file(text,bigint,bigint,boolean)', 'pg_stat_file(text,boolean)',
                  'pg_ls_dir(text,boolean,boolean)', 'lo_import(text)', 'lo_import(text,oid)', 'lo_export(oid,text)']) f;
CREATE TABLE gsf_t (a int);
GRANT ALL ON gsf_t TO gsf_user, gsf_reader, gsf_noinherit;
SET ROLE gsf_user;
SELECT pg_read_file('postmaster.pid');
SELECT pg_read_binary_file('postmaster.pid');
SELECT pg_stat_file('postmaster.pid');
SELECT pg_ls_dir('.');
SELECT lo_import('/etc/hostname');
COPY gsf_t FROM '/etc/hostname';
COPY gsf_t TO '/tmp/gsf_out';
COPY gsf_t FROM PROGRAM 'cat';
COPY (SELECT 1) TO PROGRAM 'cat';
RESET ROLE;
-- Membership without INHERIT doesn't pass the privilege on.
SET ROLE gsf_noinherit;
COPY gsf_t FROM '/etc/hostname';
RESET ROLE;
SET ROLE gsf_reader;
COPY gsf_t TO '/tmp/gsf_out';
COPY gsf_t FROM PROGRAM 'cat';
RESET ROLE;
-- The length is checked before the file.
SELECT pg_read_file('does not exist', 0, -1);
SELECT pg_read_binary_file('does not exist', 0, -1, true);
DROP TABLE gsf_t;
DROP ROLE gsf_user, gsf_reader, gsf_noinherit;
