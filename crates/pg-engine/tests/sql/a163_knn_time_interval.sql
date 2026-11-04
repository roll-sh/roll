-- btree_gist KNN over time and interval columns reads nearest-first (agent 163): the grid
-- places them (intervals by their span, as interval_cmp orders them), so `ORDER BY col <->
-- value LIMIT n` reads only the nearest rows. Ties in distance, equal spans of different
-- forms, infinite intervals, NULLs, own writes, a filter and a LIMIT past the rows.
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE a163_kt (id int, t time, iv interval);
INSERT INTO a163_kt SELECT g, time '00:00' + (g * 37 % 1440) * interval '1 minute' + (g % 7) * interval '1 second',
  (g % 50) * interval '1 day' + (g % 13) * interval '1 hour' + (g % 3) * interval '1 month'
  FROM generate_series(1, 3000) g;
INSERT INTO a163_kt VALUES (3001, NULL, NULL), (3002, '12:00', 'infinity'), (3003, '12:00', '-infinity'),
  (3004, '23:59:59.999999', '30 days'), (3005, '00:00', '1 mon'), (3006, '24:00', '720 hours');
CREATE INDEX a163_kt_t ON a163_kt USING gist (t);
CREATE INDEX a163_kt_iv ON a163_kt USING gist (iv);
ANALYZE a163_kt;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM a163_kt ORDER BY t <-> '12:34:56' LIMIT 5;
SELECT id, t, t <-> '12:34:56' AS d FROM a163_kt ORDER BY t <-> '12:34:56', id LIMIT 7;
SELECT id, t FROM a163_kt ORDER BY t <-> '00:00:01' LIMIT 6;
SELECT id, t FROM a163_kt ORDER BY t <-> '23:59:59' LIMIT 6;
SELECT id FROM a163_kt WHERE id % 2 = 0 ORDER BY t <-> '06:00' LIMIT 4;
EXPLAIN (COSTS OFF) SELECT id FROM a163_kt ORDER BY iv <-> '10 days' LIMIT 5;
SELECT id, iv, iv <-> '10 days' AS d FROM a163_kt ORDER BY iv <-> '10 days', id LIMIT 8;
SELECT id, iv FROM a163_kt ORDER BY iv <-> '1 mon', id LIMIT 6;
SELECT id, iv FROM a163_kt ORDER BY iv <-> '30 days 00:00:01', id LIMIT 6;
SELECT id, iv FROM a163_kt ORDER BY iv <-> '-5 days', id LIMIT 5;
SELECT id, iv FROM a163_kt ORDER BY iv <-> '1000 years', id LIMIT 4;
SELECT count(*) FROM (SELECT id FROM a163_kt ORDER BY iv <-> '2 days' LIMIT 5000) s;
SELECT id FROM a163_kt WHERE iv < '3 days' ORDER BY iv <-> '1 day', id LIMIT 5;
SELECT id FROM a163_kt WHERE t >= '10:00' AND t < '10:30' ORDER BY id LIMIT 5;
BEGIN;
INSERT INTO a163_kt VALUES (4000, '12:34:56', '10 days');
DELETE FROM a163_kt WHERE id = 3004;
SELECT id FROM a163_kt ORDER BY t <-> '12:34:56' LIMIT 3;
SELECT id FROM a163_kt ORDER BY iv <-> '30 days', id LIMIT 3;
ROLLBACK;
SELECT id FROM a163_kt ORDER BY iv <-> '30 days', id LIMIT 3;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a163_kt;
