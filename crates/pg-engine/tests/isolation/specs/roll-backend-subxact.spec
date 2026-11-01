# Backend status is a transaction snapshot shared by all monitoring functions.
setup
{
  CREATE TABLE sx (i int);
  CREATE FUNCTION sx_self() RETURNS TABLE (subxact_count int, subxact_overflowed bool)
    LANGUAGE SQL AS $$ SELECT s.* FROM pg_stat_get_backend_idset() bid,
      LATERAL pg_stat_get_backend_subxact(bid) s
      WHERE pg_stat_get_backend_pid(bid) = pg_backend_pid() $$;
}
teardown
{
  DROP FUNCTION sx_self();
  DROP TABLE sx;
}
session s1
setup { DROP TABLE IF EXISTS my_bid; CREATE TEMP TABLE my_bid AS SELECT bid FROM pg_stat_get_backend_idset() bid WHERE pg_stat_get_backend_pid(bid) = pg_backend_pid(); }
step none { SET stats_fetch_consistency = none; }
step cache { SET stats_fetch_consistency = cache; }
step snapshot { SET stats_fetch_consistency = snapshot; }
step begin { BEGIN; }
step write { SAVEPOINT root; INSERT INTO sx VALUES (1); }
step read { SELECT * FROM sx_self(); }
step rollback { ROLLBACK TO root; }
step clear { SELECT pg_stat_clear_snapshot(); }
step release { RELEASE root; }
step commit { COMMIT; }
step abort { ROLLBACK; }
step newwrite { INSERT INTO sx VALUES (2); }
step timestamp { SELECT pg_stat_get_snapshot_timestamp() IS NULL AS no_cumulative_snapshot; }
step cumulative { SELECT pg_stat_get_db_xact_commit(oid) IS NOT NULL FROM pg_database WHERE datname = current_database(); }
step overflow { SAVEPOINT root;
SAVEPOINT child; INSERT INTO sx VALUES (0); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (1); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (2); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (3); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (4); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (5); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (6); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (7); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (8); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (9); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (10); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (11); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (12); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (13); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (14); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (15); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (16); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (17); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (18); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (19); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (20); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (21); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (22); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (23); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (24); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (25); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (26); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (27); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (28); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (29); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (30); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (31); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (32); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (33); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (34); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (35); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (36); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (37); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (38); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (39); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (40); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (41); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (42); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (43); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (44); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (45); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (46); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (47); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (48); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (49); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (50); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (51); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (52); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (53); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (54); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (55); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (56); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (57); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (58); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (59); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (60); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (61); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (62); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (63); RELEASE child;
SAVEPOINT child; INSERT INTO sx VALUES (64); RELEASE child; }
step uncached { SAVEPOINT tail; INSERT INTO sx VALUES (999); }
step rollbacktail { ROLLBACK TO tail; }
step freeze_pid { SELECT pg_stat_get_backend_pid(bid) IS NOT NULL FROM my_bid; }
step freeze_dbid { SELECT pg_stat_get_backend_dbid(bid) IS NOT NULL FROM my_bid; }
step freeze_userid { SELECT pg_stat_get_backend_userid(bid) IS NOT NULL FROM my_bid; }
step freeze_activity { SELECT pg_stat_get_backend_activity(bid) IS NOT NULL FROM my_bid; }
step freeze_waittype { SELECT pg_stat_get_backend_wait_event_type(bid) IS NULL FROM my_bid; }
step freeze_wait { SELECT pg_stat_get_backend_wait_event(bid) IS NULL FROM my_bid; }
step freeze_xactstart { SELECT pg_stat_get_backend_xact_start(bid) IS NOT NULL FROM my_bid; }
step freeze_querystart { SELECT pg_stat_get_backend_activity_start(bid) IS NOT NULL FROM my_bid; }
step freeze_backendstart { SELECT pg_stat_get_backend_start(bid) IS NOT NULL FROM my_bid; }
step freeze_addr { SELECT pg_stat_get_backend_client_addr(bid) IS NOT NULL FROM my_bid; }
step freeze_port { SELECT pg_stat_get_backend_client_port(bid) IS NOT NULL FROM my_bid; }
step freeze_idset { SELECT count(*) > 0 FROM pg_stat_get_backend_idset(); }
step freeze_rows { SELECT count(*) = 1 FROM pg_stat_get_activity(pg_backend_pid()); }
step freeze_io { SELECT count(*) > 0 FROM pg_stat_get_backend_io(pg_backend_pid()); }
step freeze_wal { SELECT count(*) > 0 FROM pg_stat_get_backend_wal(pg_backend_pid()); }
step freeze_numbackends { SELECT pg_stat_get_db_numbackends(oid) > 0 FROM pg_database WHERE datname = current_database(); }
step freeze_progress { SELECT count(*) FROM pg_stat_get_progress_info('VACUUM'); }
step save_setting { SAVEPOINT setting; SET LOCAL stats_fetch_consistency = none; }
step undo_setting { ROLLBACK TO setting; }
step invalid_io { SELECT count(*) FROM pg_stat_get_backend_io(-1); }
step invalid_wal { SELECT count(*) FROM pg_stat_get_backend_wal(-1); }
step invalid_subxact { SELECT * FROM pg_stat_get_backend_subxact(-1); }
step fixed_stats { SELECT count(*) > 0 FROM pg_stat_get_wal(); }
permutation none begin write read release read clear read commit read
permutation none begin write read rollback read clear read abort read
permutation cache begin write read release read clear read commit read
permutation cache begin write read rollback read clear read abort read
permutation snapshot begin write read release read clear read commit read
permutation snapshot begin write read rollback read clear read abort read
permutation cache begin freeze_pid write read clear read abort read
permutation cache begin freeze_dbid write read clear read abort read
permutation cache begin freeze_userid write read clear read abort read
permutation cache begin freeze_activity write read clear read abort read
permutation cache begin freeze_waittype write read clear read abort read
permutation cache begin freeze_wait write read clear read abort read
permutation cache begin freeze_xactstart write read clear read abort read
permutation cache begin freeze_querystart write read clear read abort read
permutation cache begin freeze_backendstart write read clear read abort read
permutation cache begin freeze_addr write read clear read abort read
permutation cache begin freeze_port write read clear read abort read
permutation cache begin freeze_idset write read clear read abort read
permutation cache begin freeze_rows write read clear read abort read
permutation cache begin freeze_io write read clear read abort read
permutation cache begin freeze_wal write read clear read abort read
permutation none begin overflow read uncached clear read rollbacktail clear read rollback clear read newwrite clear read release clear read abort read
permutation cache begin overflow read uncached clear read rollbacktail clear read rollback clear read newwrite clear read release clear read abort read
permutation snapshot begin overflow read uncached clear read rollbacktail clear read rollback clear read newwrite clear read release clear read abort read
permutation cache begin freeze_pid write none read timestamp read abort
permutation none begin freeze_pid write snapshot read cumulative read abort
permutation cache begin freeze_pid write cache timestamp read abort
permutation cache begin freeze_pid write none cache timestamp read abort
permutation cache begin freeze_pid write none clear read timestamp read abort
permutation cache begin freeze_numbackends write read clear read abort
permutation cache begin freeze_progress write read clear read abort
permutation cache begin freeze_pid write save_setting undo_setting read timestamp read abort
permutation cache begin invalid_io write read abort
permutation cache begin invalid_wal write read abort
permutation cache begin invalid_subxact write read clear read abort
permutation cache begin freeze_pid write none read fixed_stats read abort
