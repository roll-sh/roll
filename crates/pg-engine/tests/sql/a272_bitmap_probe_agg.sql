-- A parallel bitmap heap scan's rows probed into a hash join and aggregated from the batch's
-- columns (TPC-H q14's shape: lineitem rows of a date range, each joined to its part, the
-- joined columns past the last one the probe reads): inner joins, duplicate and missing inner
-- keys, NULL outer keys and NULL aggregated values, a string kept from the outer row, rows
-- stored before ADD COLUMN, against the rows PostgreSQL gives.
CREATE TABLE a272_p (id int, kind text, w int) WITH (autovacuum_enabled = false);
INSERT INTO a272_p SELECT g, CASE WHEN g % 3 = 0 THEN 'PROMO ' || g ELSE 'STD ' || g END, g % 11 FROM generate_series(1, 2000) g;
INSERT INTO a272_p SELECT g, 'DUP ' || g, 100 + g % 4 FROM generate_series(1, 2000, 89) g;
CREATE TABLE a272_l (k int8, pid int, d date, qty int, price numeric(12,2), disc numeric(4,2), note varchar(40)) WITH (autovacuum_enabled = false);
INSERT INTO a272_l SELECT g,
  CASE WHEN g % 199 = 0 THEN NULL ELSE (g * 7) % 2300 END,
  date '1995-01-01' + (g % 365),
  g % 50,
  CASE WHEN g % 17 = 0 THEN NULL ELSE (g % 1000) * 1.25 END,
  (g % 10) * 0.01,
  'note ' || (g % 13)
  FROM generate_series(1, 30000) g;
ALTER TABLE a272_l ADD COLUMN tag text DEFAULT 'old';
INSERT INTO a272_l SELECT g, (g * 11) % 2300, date '1995-01-01' + (g % 365), g % 50, (g % 700) * 0.5, (g % 7) * 0.01, 'late ' || (g % 5), 'new'
  FROM generate_series(30001, 40000) g;
CREATE INDEX a272_l_d ON a272_l (d);
VACUUM ANALYZE a272_p;
VACUUM ANALYZE a272_l;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET enable_seqscan = off;
SET enable_indexscan = off;
SELECT 100.00 * sum(CASE WHEN p.kind LIKE 'PROMO%' THEN l.price * (1 - l.disc) ELSE 0 END) / sum(l.price * (1 - l.disc)) AS promo,
  count(*), count(l.price)
  FROM a272_l l, a272_p p WHERE l.pid = p.id AND l.d >= date '1995-03-01' AND l.d < date '1995-03-01' + interval '4' month;
SELECT p.w, count(*), sum(l.price), sum(l.qty), min(l.note), max(l.tag)
  FROM a272_l l JOIN a272_p p ON l.pid = p.id WHERE l.d >= date '1995-02-01' AND l.d < date '1995-07-01'
  GROUP BY p.w ORDER BY 1;
SELECT l.tag, l.note, count(*), sum(l.price * (1 - l.disc)), max(p.id)
  FROM a272_l l JOIN a272_p p ON l.pid = p.id WHERE l.d BETWEEN date '1995-05-01' AND date '1995-09-30'
  GROUP BY l.tag, l.note ORDER BY 1, 2;
SELECT count(*), sum(l.price) FROM a272_l l JOIN a272_p p ON l.pid = p.id
  WHERE l.d >= date '1995-04-01' AND l.d < date '1995-10-01' AND p.kind LIKE 'DUP%';
DROP TABLE a272_l;
DROP TABLE a272_p;
