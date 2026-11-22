-- An on-access prune (heap_page_prune_opt, here a sequential scan reading the page) takes the
-- dead tuples it reclaims off n_dead_tup (pgstat_update_heap_dead_tuples): a HOT chain's root
-- redirected or its heap-only versions freed count, a line pointer left dead (an old version
-- index entries still point to, for VACUUM) doesn't. Page 0 is full (fillfactor 100), so the
-- first update moves its row; the others are heap-only in the space the prunes free.
CREATE TABLE a290_prune (id int PRIMARY KEY, v int, pad text) WITH (autovacuum_enabled = off, fillfactor = 100);
INSERT INTO a290_prune SELECT i, 0, repeat('x', 100) FROM generate_series(1, 200) i;
UPDATE a290_prune SET v = v + 1 WHERE id = 1;
SELECT count(*) FROM a290_prune;
UPDATE a290_prune SET v = v + 1 WHERE id = 2;
SELECT count(*) FROM a290_prune;
UPDATE a290_prune SET v = v + 1 WHERE id = 3;
SELECT count(*) FROM a290_prune;
SELECT pg_stat_force_next_flush();
SELECT n_tup_upd, n_tup_hot_upd, n_dead_tup FROM pg_stat_user_tables WHERE relname = 'a290_prune';
SELECT ctid, id FROM a290_prune WHERE id <= 3 ORDER BY id;
DROP TABLE a290_prune;
