-- GUC_SUPERUSER_ONLY parameters need pg_read_all_settings' privileges to be examined, and
-- function EXECUTE privileges are checked a call before its arguments (agent 97).
DROP ROLE IF EXISTS regress_a97_plain;
CREATE ROLE regress_a97_plain;
SET ROLE regress_a97_plain;
SHOW hba_file;
SHOW data_directory;
SHOW Log_Directory;
SELECT current_setting('config_file');
SELECT current_setting('ssl_ciphers', true);
SELECT current_setting('no_such_setting', true) IS NULL;
SELECT name FROM pg_settings WHERE name IN ('hba_file', 'log_directory', 'ssl_ciphers', 'port') ORDER BY 1;
SELECT count(*) FROM pg_show_all_settings() WHERE name = 'unix_socket_directories';
SHOW log_destination;
SELECT pg_stat_file(pg_current_logfile());
SELECT pg_read_file(pg_current_logfile('csvlog'));
SELECT length(pg_current_logfile());
RESET ROLE;
GRANT pg_read_all_settings TO regress_a97_plain;
SET ROLE regress_a97_plain;
SELECT name FROM pg_settings WHERE name IN ('hba_file', 'log_directory', 'ssl_ciphers', 'port') ORDER BY 1;
SHOW unix_socket_directories;
SHOW log_filename;
RESET ROLE;
SELECT pg_current_logfile(), pg_current_logfile('csvlog'), pg_current_logfile(NULL);
SELECT pg_current_logfile('foo');
SELECT pg_rotate_logfile();
DROP ROLE regress_a97_plain;
