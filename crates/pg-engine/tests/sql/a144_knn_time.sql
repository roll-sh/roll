-- btree_gist KNN over timestamp and timestamptz reads nearest-first (agent 144): the
-- distances are intervals, ordered by their span. Ties, NULLs, infinities, an index
-- condition, LIMIT and own writes; the rows are compared with a sort of the same distances.
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE a144_kt (id int, ts timestamp, tz timestamptz);
INSERT INTO a144_kt SELECT g, timestamp '2026-01-01' + (g * 7919 % 5000) * interval '37 minutes',
  timestamptz '2026-01-01 00:00+00' + (g * 104729 % 5000) * interval '11 minutes' FROM generate_series(1, 5000) g;
INSERT INTO a144_kt VALUES (5001, NULL, NULL), (5002, 'infinity', 'infinity'), (5003, '-infinity', '-infinity'),
  (5004, '2026-03-01 12:00', '2026-03-01 12:00+00'), (5005, '2026-03-01 12:00', '2026-03-01 12:00+00');
CREATE INDEX a144_kt_ts ON a144_kt USING gist (ts);
CREATE INDEX a144_kt_tz ON a144_kt USING gist (tz);
ANALYZE a144_kt;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SET timezone = 'UTC';
EXPLAIN (COSTS OFF) SELECT id FROM a144_kt ORDER BY ts <-> '2026-03-01 12:00' LIMIT 5;
SELECT id, ts, ts <-> '2026-03-01 12:00' AS d FROM a144_kt ORDER BY ts <-> '2026-03-01 12:00', id LIMIT 8;
SELECT ts <-> '2026-03-01 12:00' AS d FROM a144_kt ORDER BY ts <-> '2026-03-01 12:00' LIMIT 8;
SELECT ts <-> '2026-02-15 03:30' AS d FROM a144_kt ORDER BY ts <-> '2026-02-15 03:30' LIMIT 10;
SELECT tz <-> '2026-01-20 05:00+00' AS d FROM a144_kt ORDER BY tz <-> '2026-01-20 05:00+00' LIMIT 10;
SELECT tz <-> '2026-01-20 05:00+05' AS d FROM a144_kt ORDER BY tz <-> '2026-01-20 05:00+05' LIMIT 3;
SELECT id, ts <-> '2100-01-01' AS d FROM a144_kt ORDER BY ts <-> '2100-01-01' LIMIT 4;
SELECT id, ts <-> '1900-01-01' AS d FROM a144_kt ORDER BY ts <-> '1900-01-01' LIMIT 4;
SELECT ts <-> '2026-02-15 03:30' AS d FROM a144_kt WHERE ts > '2026-02-15' ORDER BY ts <-> '2026-02-15 03:30' LIMIT 5;
SELECT ts <-> '2026-03-01 12:00' AS d FROM a144_kt WHERE id > 5000 ORDER BY ts <-> '2026-03-01 12:00' LIMIT 10;
SELECT count(*) FROM (SELECT id FROM a144_kt ORDER BY ts <-> '2026-02-01' LIMIT 6000) s;
BEGIN;
INSERT INTO a144_kt VALUES (6000, '2026-02-15 03:31', NULL);
SELECT id, ts <-> '2026-02-15 03:30' AS d FROM a144_kt ORDER BY ts <-> '2026-02-15 03:30' LIMIT 3;
ROLLBACK;
RESET timezone;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a144_kt;
