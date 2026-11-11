-- Monitoring and administration functions: WAL positions, recovery and backup control,
-- the control file, the cumulative statistics' shapes and pg_monitor's privileges
-- (round 20, agent 80b). Values that differ between servers are compared, not shown.
DROP ROLE IF EXISTS regress_a80b_mon;
DROP ROLE IF EXISTS regress_a80b_plain;
SELECT pg_typeof(pg_current_wal_lsn()), pg_typeof(pg_current_wal_insert_lsn()), pg_typeof(pg_current_wal_flush_lsn());
SELECT pg_current_wal_flush_lsn() <= pg_current_wal_insert_lsn(), pg_current_wal_lsn() >= '0/1000000';
SELECT pg_wal_lsn_diff('0/10', '0/0'), pg_wal_lsn_diff('1/0', '0/FF'), pg_wal_lsn_diff('0/0', '0/10'), pg_typeof(pg_wal_lsn_diff('0/1', '0/0'));
SELECT pg_wal_lsn_diff(NULL, '0/0') IS NULL;
SELECT pg_walfile_name(pg_current_wal_lsn()) ~ '^0000000100000000000000[0-9A-F]{2}$';
SELECT pg_is_in_recovery(), pg_last_wal_receive_lsn(), pg_last_wal_replay_lsn(), pg_last_xact_replay_timestamp();
SELECT pg_is_wal_replay_paused();
SELECT pg_get_wal_replay_pause_state();
SELECT pg_wal_replay_pause();
SELECT pg_wal_replay_resume();
SELECT pg_promote();
SELECT pg_promote(false, 5);
-- A WAL switch moves the position to the next segment's start, where another does nothing.
SELECT pg_switch_wal() <= pg_current_wal_lsn() AS switched;
SELECT pg_current_wal_lsn() = pg_switch_wal() AS at_segment_start;
SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn) % (16 * 1024 * 1024) = 0 AS segment_boundary;
SELECT pg_create_restore_point('a80b') > '0/0';
SELECT pg_create_restore_point(repeat('y', 64));
SELECT pg_log_standby_snapshot() > '0/0';
-- Backups.
SELECT pg_backup_stop();
SELECT pg_backup_start(repeat('x', 1100));
SELECT pg_backup_start('a80b', true) = (SELECT redo_lsn FROM pg_control_checkpoint());
SELECT pg_backup_start('again');
SELECT lsn > (SELECT checkpoint_lsn FROM pg_control_checkpoint()), regexp_replace(labelfile, '[0-9A-F]+/[0-9A-F]+|[0-9A-F]{24}|[0-9-]+ [0-9:]+ UTC', 'X', 'g'), spcmapfile FROM pg_backup_stop(false);
SELECT pg_backup_stop(false);
SELECT pg_backup_start('a80b2', false) IS NOT NULL;
SELECT labelfile ~ 'LABEL: a80b2' FROM pg_backup_stop();
-- The control file, as of the last checkpoint.
CHECKPOINT;
SELECT redo_lsn <= checkpoint_lsn AS redo_first, checkpoint_lsn <= pg_current_wal_lsn() AS written, redo_wal_file = pg_walfile_name(redo_lsn + 1) AS redo_file, timeline_id, prev_timeline_id, full_page_writes, next_xid ~ '^0:[0-9]+$' AS next_xid, oldest_xid_dbid, oldest_multi_dbid, oldest_commit_ts_xid, newest_commit_ts_xid, checkpoint_time <= now() AS past FROM pg_control_checkpoint();
SELECT pg_control_version, catalog_version_no, pg_control_last_modified = (SELECT checkpoint_time FROM pg_control_checkpoint()) AS at_checkpoint FROM pg_control_system();
SELECT * FROM pg_control_init();
SELECT * FROM pg_control_recovery();
SELECT pg_postmaster_start_time() <= now(), pg_backend_pid() > 0, pg_jit_available() IN (true, false);
-- WAL summaries (summarize_wal is off) and the resource managers.
SELECT * FROM pg_get_wal_summarizer_state();
SELECT count(*) FROM pg_available_wal_summaries();
SELECT * FROM pg_wal_summary_contents(1, '0/0', '0/1');
SELECT * FROM pg_get_wal_resource_managers();
-- The cumulative statistics' fixed views: one row, counters, and their reset times.
SELECT num_timed >= 0, num_requested > 0, num_done > 0, restartpoints_timed, restartpoints_req, restartpoints_done, write_time >= 0, sync_time >= 0, buffers_written >= 0, slru_written >= 0, stats_reset <= now() FROM pg_stat_checkpointer;
SELECT wal_records >= 0, wal_fpi >= 0, wal_bytes >= 0, wal_buffers_full >= 0, stats_reset <= now() FROM pg_stat_wal;
SELECT buffers_clean, maxwritten_clean, buffers_alloc >= 0, stats_reset <= now() FROM pg_stat_bgwriter;
SELECT archived_count, last_archived_wal, last_archived_time, failed_count, last_failed_wal, last_failed_time, stats_reset <= now() FROM pg_stat_archiver;
SELECT count(*), count(DISTINCT (backend_type, object, context)) FROM pg_stat_io;
SELECT datname, numbackends > 0 AS connected, xact_commit >= 0, checksum_failures, checksum_last_failure, stats_reset IS NOT NULL AS reset_set FROM pg_stat_database WHERE datname = current_database();
SELECT datname IS NULL AS shared, numbackends, checksum_failures FROM pg_stat_database WHERE datid = 0;
SELECT confl_tablespace, confl_lock, confl_snapshot, confl_bufferpin, confl_deadlock, confl_active_logicalslot FROM pg_stat_database_conflicts WHERE datname = current_database();
SELECT pg_stat_get_db_numbackends(0), pg_stat_get_db_numbackends(1) >= 0, pg_stat_get_db_checksum_failures(0), pg_stat_get_db_checksum_last_failure(0);
-- pg_monitor's members read every backend's statistics and settings; others don't run
-- the server's administration functions.
CREATE ROLE regress_a80b_mon;
CREATE ROLE regress_a80b_plain;
GRANT pg_monitor TO regress_a80b_mon;
SELECT pg_has_role('regress_a80b_mon', 'pg_read_all_stats', 'USAGE'), pg_has_role('regress_a80b_mon', 'pg_read_all_settings', 'MEMBER'), pg_has_role('regress_a80b_mon', 'pg_stat_scan_tables', 'SET');
SET ROLE regress_a80b_mon;
SELECT count(*) > 0 FROM pg_stat_activity WHERE backend_type = 'checkpointer' AND query IS NOT NULL;
SELECT count(*) >= 0 FROM pg_ls_waldir();
SELECT count(*) >= 0 FROM pg_ls_archive_statusdir();
SELECT pg_switch_wal();
RESET ROLE;
SET ROLE regress_a80b_plain;
SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'checkpointer' AND query = '<insufficient privilege>';
SELECT pg_switch_wal();
SELECT pg_backup_start('x');
SELECT pg_create_restore_point('x');
SELECT pg_wal_replay_pause();
SELECT pg_ls_waldir();
SELECT pg_current_wal_lsn() > '0/0', pg_is_in_recovery();
RESET ROLE;
DROP ROLE regress_a80b_mon;
DROP ROLE regress_a80b_plain;
