# Snapshot raw metadata and subxids; apply privileges and read wait events at read time.
setup
{
  CREATE TABLE sx (i int);
  CREATE ROLE sx_reader;
  CREATE FUNCTION sx_other() RETURNS TABLE (subxact_count int, subxact_overflowed bool)
    LANGUAGE SQL AS $$ SELECT s.* FROM pg_stat_get_backend_idset() bid,
      LATERAL pg_stat_get_backend_subxact(bid) s
      WHERE pg_stat_get_backend_pid(bid) = (SELECT pid FROM pg_stat_activity WHERE application_name = 's2') $$;
}
teardown
{
  DROP FUNCTION sx_other();
  DROP TABLE sx;
  DROP ROLE sx_reader;
}
session s1
step none { SET stats_fetch_consistency = none; }
step cache { SET stats_fetch_consistency = cache; }
step snapshot { SET stats_fetch_consistency = snapshot; }
step begin { BEGIN; }
step read { SELECT * FROM sx_other(); }
step freeze { SELECT state, query = 'BEGIN;' AS began, backend_xid IS NULL AS no_xid FROM pg_stat_activity WHERE application_name = 's2'; }
step metadata { SELECT state, query = 'BEGIN;' AS began, backend_xid IS NULL AS no_xid FROM pg_stat_activity WHERE application_name = 's2'; }
step clear { SELECT pg_stat_clear_snapshot(); }
step commit { COMMIT; }
step restricted { SET ROLE sx_reader; }
step unrestrict { RESET ROLE; }
step mask { SELECT query = '<insufficient privilege>' AS masked, state IS NULL AS hidden FROM pg_stat_activity WHERE application_name = 's2'; }
step visible { SELECT query <> '<insufficient privilege>' AS visible, state IS NOT NULL AS visible_state FROM pg_stat_activity WHERE application_name = 's2'; }
step current_wait { SELECT state = 'idle in transaction' AS frozen_state, wait_event = 'relation' AS live_wait FROM pg_stat_activity WHERE application_name = 's2'; }
step lock { LOCK TABLE sx IN ACCESS EXCLUSIVE MODE; }
step undo { ROLLBACK; }
step hold { LOCK TABLE sx IN ACCESS EXCLUSIVE MODE; }
session s2
setup { SET application_name = 's2'; }
step writer_begin { BEGIN; }
step writer_write { SAVEPOINT root; INSERT INTO sx VALUES (1); }
step writer_rollback { ROLLBACK TO root; }
step writer_release { RELEASE root; }
step writer_commit { COMMIT; }
step writer_abort { ROLLBACK; }
step writer_select { SELECT * FROM sx; }

step writer_overflow { SAVEPOINT root;
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
SAVEPOINT child; INSERT INTO sx VALUES (64); RELEASE child;
}
step writer_newwrite { INSERT INTO sx VALUES (100); }
step writer_error { SELECT 1 / 0; }
step writer_topwrite { INSERT INTO sx VALUES (999); }

permutation none writer_begin begin freeze writer_write read metadata clear read writer_rollback read clear read writer_abort read commit read
permutation cache writer_begin begin freeze writer_write read metadata clear read writer_release read clear read writer_commit read commit read
permutation snapshot writer_begin begin writer_write read writer_rollback read clear read writer_abort commit read
# Permission checks use the reader's current role, regardless of who took the snapshot.
permutation writer_begin begin freeze restricted mask unrestrict visible restricted mask unrestrict commit writer_abort
permutation writer_begin begin restricted mask unrestrict visible restricted mask unrestrict commit writer_abort
# A wait starts after the metadata snapshot. The waiter is canceled by releasing the lock.
permutation writer_begin begin freeze hold writer_select current_wait undo writer_abort
permutation none writer_begin writer_overflow begin read writer_rollback read clear read writer_newwrite clear read writer_abort read commit read
permutation cache writer_begin writer_overflow begin read writer_rollback read clear read writer_newwrite clear read writer_abort read commit read
permutation snapshot writer_begin writer_overflow begin read writer_rollback read clear read writer_newwrite clear read writer_abort read commit read
# AbortSubTransaction removes the failed child's cached IDs before ROLLBACK TO.
permutation writer_begin writer_write begin read writer_error read clear read writer_rollback writer_newwrite clear read writer_abort commit
# AbortTransaction resets PGPROC while the failed SQL block still awaits ROLLBACK.
permutation writer_begin writer_overflow writer_release writer_topwrite begin read writer_error read clear read writer_abort commit read
