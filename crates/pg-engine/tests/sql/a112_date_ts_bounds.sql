-- A date column compared with timestamps: an index range from the timestamps' dates,
-- the comparisons checked on each row (midnight and not, both directions, infinities).
CREATE TABLE a112_d (id int, d date);
INSERT INTO a112_d SELECT i, date '2020-01-01' + i FROM generate_series(1, 2000) i;
CREATE INDEX a112_d_d ON a112_d (d);
ANALYZE a112_d;
SET enable_seqscan = off;
SELECT count(*), min(d), max(d) FROM a112_d WHERE d < timestamp '2021-01-01 00:00';
SELECT count(*), min(d), max(d) FROM a112_d WHERE d < timestamp '2021-01-01 12:00';
SELECT count(*), min(d), max(d) FROM a112_d WHERE d <= timestamp '2021-01-01 00:00';
SELECT count(*), min(d), max(d) FROM a112_d WHERE d <= timestamp '2021-01-01 12:00';
SELECT count(*), min(d), max(d) FROM a112_d WHERE d > timestamp '2021-01-01 00:00' AND d <= timestamp '2021-01-03 00:00';
SELECT count(*), min(d), max(d) FROM a112_d WHERE d > timestamp '2021-01-01 12:00' AND d < timestamp '2021-01-05 00:00';
SELECT count(*), min(d), max(d) FROM a112_d WHERE d >= timestamp '2021-01-01 12:00' AND d < date '2021-01-01' + interval '1' month;
SELECT count(*), min(d), max(d) FROM a112_d WHERE d >= timestamp '2021-01-01 00:00' AND d < date '2021-01-01' + interval '1' month;
SELECT id, d FROM a112_d WHERE d BETWEEN timestamp '2020-12-30 06:00' AND timestamp '2021-01-02 00:00' ORDER BY d DESC;
SELECT count(*) FROM a112_d WHERE d < timestamp 'infinity';
SELECT count(*) FROM a112_d WHERE d > timestamp '-infinity' AND d < timestamp '2020-01-05';
SELECT count(*) FROM a112_d WHERE d < timestamp '-infinity';
RESET enable_seqscan;
DROP TABLE a112_d;
