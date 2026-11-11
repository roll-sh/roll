-- Index scans read only what they return: backward scans, LIMIT, ANY, cursors, min/max,
-- statistics and LP_DEAD marking of the entries a scan passes (agent 103).
CREATE TABLE a103_t (id int PRIMARY KEY, k int, v text);
CREATE INDEX a103_t_k ON a103_t (k);
CREATE INDEX a103_t_kd ON a103_t (k DESC NULLS LAST, id);
INSERT INTO a103_t SELECT g, g % 7, 'v' || g FROM generate_series(1, 2000) g;
INSERT INTO a103_t VALUES (2001, NULL, 'null1'), (2002, NULL, 'null2');
-- Equal keys whose rows moved: heap order differs from key order.
UPDATE a103_t SET v = v || 'x' WHERE id % 5 = 0 AND k = 3;
VACUUM ANALYZE a103_t;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT id, k FROM a103_t ORDER BY id DESC LIMIT 5;
SELECT id, k FROM a103_t WHERE id < 1500 ORDER BY id DESC LIMIT 3;
SELECT id, k FROM a103_t WHERE id <= 1500 AND id > 1490 ORDER BY id DESC;
SELECT id, v FROM a103_t WHERE k = 3 ORDER BY k DESC LIMIT 6;
SELECT id, v FROM a103_t WHERE k = 3 ORDER BY k LIMIT 6;
SELECT id, k FROM a103_t WHERE k IN (2, 5) ORDER BY k DESC LIMIT 4;
SELECT id, k FROM a103_t WHERE id = ANY (ARRAY[5, 1999, 77, 1000]) ORDER BY id DESC;
SELECT id, k FROM a103_t WHERE id = ANY (ARRAY[5, 1999, 77, 1000]) ORDER BY id LIMIT 2;
SELECT k, id FROM a103_t ORDER BY k DESC NULLS LAST, id LIMIT 3;
SELECT k, id FROM a103_t ORDER BY k NULLS FIRST, id DESC LIMIT 3;
SELECT k, id FROM a103_t WHERE k IS NULL ORDER BY k DESC NULLS LAST, id DESC;
SELECT min(id), max(id), max(k), min(k) FROM a103_t;
SELECT max(id) FROM a103_t WHERE id < 1000;
SELECT EXISTS (SELECT 1 FROM a103_t WHERE k = 6 AND id > 1990);
-- Backward scans in a nested loop's inner side, rescanned with each outer row.
SELECT o.x, (SELECT t.id FROM a103_t t WHERE t.id < o.x ORDER BY t.id DESC LIMIT 1) FROM (VALUES (10), (1500), (1)) o(x);
SELECT o.x, t.id FROM (VALUES (3), (5)) o(x) CROSS JOIN LATERAL (SELECT id FROM a103_t WHERE k = o.x ORDER BY k DESC, id DESC LIMIT 2) t;
-- A cursor reading both ways over a backward scan.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id FROM a103_t WHERE id > 1990 ORDER BY id DESC;
FETCH 3 FROM c;
FETCH BACKWARD 2 FROM c;
FETCH LAST FROM c;
FETCH PRIOR FROM c;
FETCH ABSOLUTE 2 FROM c;
COMMIT;
-- Own uncommitted writes seen by a backward scan.
BEGIN;
INSERT INTO a103_t VALUES (5000, 3, 'new');
DELETE FROM a103_t WHERE id = 2000;
UPDATE a103_t SET id = 4999 WHERE id = 1999;
SELECT id, k FROM a103_t ORDER BY id DESC LIMIT 4;
SELECT id, v FROM a103_t WHERE k = 3 ORDER BY k DESC, id DESC LIMIT 2;
ROLLBACK;
-- FOR UPDATE SKIP LOCKED over a backward scan.
SELECT id FROM a103_t WHERE k = 4 ORDER BY k DESC LIMIT 2 FOR UPDATE SKIP LOCKED;
EXPLAIN (COSTS OFF) SELECT id FROM a103_t ORDER BY id DESC LIMIT 5;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT id, v FROM a103_t WHERE id < 1500 ORDER BY id DESC LIMIT 3;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a103_t;
-- Dead entries a LIMIT scan passes are marked; the rest stay for later insertions.
CREATE TABLE a103_d (id int PRIMARY KEY, pad text) WITH (autovacuum_enabled = off);
INSERT INTO a103_d SELECT g, repeat('x', 40) FROM generate_series(1, 3000) g;
DELETE FROM a103_d WHERE id <= 2900 AND id <> 1500;
SET enable_seqscan = off;
SELECT id FROM a103_d ORDER BY id LIMIT 1;
SELECT id FROM a103_d WHERE id > 2950 ORDER BY id DESC LIMIT 1;
RESET enable_seqscan;
INSERT INTO a103_d SELECT g, repeat('y', 40) FROM generate_series(1, 2900) g WHERE g <> 1500;
SELECT pg_relation_size('a103_d_pkey') / 8192 AS pages;
DROP TABLE a103_d;
-- What the statistics count for scans a LIMIT stops.
CREATE TABLE a103_s (id int PRIMARY KEY, v text);
INSERT INTO a103_s SELECT g, 'v' || g FROM generate_series(1, 100) g;
VACUUM ANALYZE a103_s;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT v FROM a103_s ORDER BY id DESC LIMIT 3;
SELECT v FROM a103_s WHERE id > 10 ORDER BY id LIMIT 2;
SELECT v FROM a103_s WHERE id < 90 ORDER BY id DESC LIMIT 2;
SELECT v FROM a103_s WHERE id = ANY (ARRAY[3, 50, 7]) ORDER BY id DESC LIMIT 2;
SELECT pg_stat_force_next_flush();
SELECT idx_scan, idx_tup_read, idx_tup_fetch FROM pg_stat_user_indexes WHERE indexrelname = 'a103_s_pkey';
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a103_s;
