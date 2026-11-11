-- pg_settings rows that differ by a show hook or a source, PL/pgSQL's parameters appearing once
-- it is loaded, and the log directory a cluster logging to stderr doesn't have (agent 80).
SELECT name, setting, source FROM pg_settings
  WHERE name IN ('archive_command', 'data_directory_mode', 'log_file_mode', 'unix_socket_permissions',
                 'transaction_isolation', 'transaction_read_only', 'transaction_deferrable', 'allow_alter_system')
  ORDER BY name;
SHOW archive_command;
SHOW log_file_mode;
SELECT name, context, vartype, boot_val, short_desc, extra_desc FROM pg_settings WHERE name = 'allow_alter_system';
SELECT count(*) FROM pg_settings WHERE name LIKE 'plpgsql.%';
DO $$ BEGIN END $$;
SELECT name, setting FROM pg_settings WHERE name LIKE 'plpgsql.%' ORDER BY name;
SELECT * FROM pg_ls_logdir();
SELECT count(*) FROM pg_ls_tmpdir();
SELECT pg_current_logfile();
