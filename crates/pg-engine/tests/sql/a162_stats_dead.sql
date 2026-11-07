-- ANALYZE reports the dead versions it finds (acquire_sample_rows' deadrows) as n_dead_tup,
-- and an INSERT that fails on a unique key still counts its heap tuple (inserted, then dead
-- with the abort). (ANALYZE right after DML whose counts this backend hasn't flushed yet:
-- PostgreSQL subtracts the pending ones, so the result depends on its flush interval.)
CREATE TABLE a162_v (id int PRIMARY KEY, v int) WITH (autovacuum_enabled = off);
INSERT INTO a162_v SELECT i, i FROM generate_series(1, 30) i;
DELETE FROM a162_v WHERE id > 10;
UPDATE a162_v SET v = 0 WHERE id < 3;
SELECT pg_stat_force_next_flush();
SELECT n_tup_ins, n_tup_upd, n_tup_del, n_live_tup, n_dead_tup, n_mod_since_analyze FROM pg_stat_user_tables WHERE relname = 'a162_v';
ANALYZE a162_v;
SELECT n_live_tup, n_dead_tup, n_mod_since_analyze, analyze_count FROM pg_stat_user_tables WHERE relname = 'a162_v';
VACUUM a162_v;
SELECT n_live_tup, n_dead_tup, vacuum_count FROM pg_stat_user_tables WHERE relname = 'a162_v';

INSERT INTO a162_v VALUES (3, 0);
INSERT INTO a162_v VALUES (100, 0), (4, 0);
BEGIN;
INSERT INTO a162_v VALUES (200, 0);
SAVEPOINT s;
INSERT INTO a162_v VALUES (1, 1);
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a162_v';
COMMIT;
SELECT pg_stat_force_next_flush();
SELECT n_tup_ins, n_live_tup, n_dead_tup, n_ins_since_vacuum FROM pg_stat_user_tables WHERE relname = 'a162_v';
DROP TABLE a162_v;
