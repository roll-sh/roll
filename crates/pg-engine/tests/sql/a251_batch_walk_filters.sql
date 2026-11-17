-- A parallel scan's batches (column decode by the declared types, filters a column at a
-- time) against the rows PostgreSQL gives: numeric and integer columns compared with
-- constants of other scales, NULLs, dates against dates, timestamps and infinities, columns
-- of many types stepped over (NULLs among them, strings past 127 bytes, numerics past 8
-- bytes), and rows stored before columns were added.
CREATE TABLE a251_w (id int, i2 smallint, q numeric(15,2), b bool, t text, ts timestamp, d date, iv interval, n numeric, by bytea, f4 real, i8 bigint, d2 date) WITH (autovacuum_enabled = false);
INSERT INTO a251_w SELECT s, (s % 300)::smallint,
  CASE WHEN s % 13 = 0 THEN NULL ELSE (s % 5000) * 0.01 END,
  CASE WHEN s % 7 = 0 THEN NULL ELSE s % 2 = 0 END,
  CASE WHEN s % 11 = 0 THEN NULL WHEN s % 17 = 0 THEN repeat('x', 130 + s % 50) ELSE 't' || s END,
  timestamp '2000-01-01' + s * interval '1 hour',
  CASE WHEN s % 19 = 0 THEN NULL WHEN s = 5 THEN 'infinity'::date WHEN s = 6 THEN '-infinity'::date ELSE date '1994-01-01' + (s % 1000) END,
  CASE WHEN s % 3 = 0 THEN NULL ELSE s * interval '1 minute' END,
  CASE WHEN s % 23 = 0 THEN NULL WHEN s % 29 = 0 THEN 123456789012345678901234.5 ELSE s * 0.001 END,
  CASE WHEN s % 31 = 0 THEN NULL ELSE decode(lpad(to_hex(s), 8, '0'), 'hex') END,
  s / 3.0, s * 1000000007::bigint,
  date '1994-06-01' + (s % 700)
  FROM generate_series(1, 120000) s;
ALTER TABLE a251_w ADD COLUMN late int DEFAULT 7;
INSERT INTO a251_w (id, q, d, d2, late) SELECT s, s * 0.5, date '1995-01-01', date '1994-01-01', CASE WHEN s % 2 = 0 THEN NULL ELSE s END FROM generate_series(120001, 121000) s;
VACUUM ANALYZE a251_w;
SELECT count(*), sum(q), sum(id) FROM a251_w WHERE q < 24;
SELECT count(*), sum(q) FROM a251_w WHERE q >= 0.05 AND q <= 0.07;
SELECT count(*), sum(q) FROM a251_w WHERE 24.5 > q;
SELECT count(*), sum(q) FROM a251_w WHERE q = 12.340;
SELECT count(*), sum(q) FROM a251_w WHERE q <> 12.3 AND q > 1.005;
SELECT count(*) FROM a251_w WHERE q < NULL;
SELECT count(*), sum(n) FROM a251_w WHERE n > 100.5;
SELECT count(*), sum(i8) FROM a251_w WHERE i8 >= 50000000350 AND i8 < 90000000000.5;
SELECT count(*), sum(id) FROM a251_w WHERE id < 1000.5 AND i2 > 150;
SELECT count(*), sum(q) FROM a251_w WHERE d >= date '1994-06-01' AND d < date '1994-06-01' + interval '1' month;
SELECT count(*), sum(q) FROM a251_w WHERE d <= date '1996-01-01' - interval '90' day;
SELECT count(*), sum(q) FROM a251_w WHERE d > timestamp '1995-06-01 12:00';
SELECT count(*), sum(q) FROM a251_w WHERE d < 'infinity'::timestamp AND d > '-infinity'::date;
SELECT count(*), sum(q) FROM a251_w WHERE d < d2;
SELECT count(*), sum(q) FROM a251_w WHERE d >= d2 AND d2 < date '1995-01-01';
SELECT count(*), sum(q), sum(late) FROM a251_w WHERE late > 3;
SELECT count(*), sum(q), count(t), count(b), count(by), count(iv), sum(f4) FROM a251_w WHERE q > 10;
SELECT count(*), sum(q * 2), avg(q) FROM a251_w WHERE t LIKE 'xxx%' OR q > 49;
SELECT b, count(*), sum(q) FROM a251_w WHERE d2 < date '1995-02-01' GROUP BY b ORDER BY b;
SELECT i2 % 3 AS g, count(*), sum(q), min(d), max(d2) FROM a251_w WHERE ts < timestamp '2010-01-01' GROUP BY 1 ORDER BY 1;
DROP TABLE a251_w;
