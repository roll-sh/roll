-- Aggregations over a parallel scan that its workers sum a batch at a time
-- (exec::partial batch_run): numeric sums and averages of typmod'd columns and of their
-- sums, differences and products, with NULLs, groups of NULLs only, no rows, negative
-- values and zeros of every scale; and what goes the row way: a column of mixed scales,
-- NaN and Infinity, products past an i128. (A filter dividing by zero is in parallel_scan:
-- PostgreSQL raises it in whichever process reads the row, with or without its CONTEXT.)
CREATE TABLE a220_n (g int, k char(1), p numeric(15,2), d numeric(15,2), m numeric, i int, nn numeric(15,2)) WITH (autovacuum_enabled = false);
INSERT INTO a220_n SELECT s, chr(65 + s % 4), (s % 100000) * 1.37, (s % 11) / 100.0,
  CASE WHEN s % 50000 = 0 THEN 'NaN' WHEN s % 70001 = 0 THEN 'Infinity' WHEN s % 3 = 0 THEN s / 8.0 ELSE s END,
  s % 1000 - 500, CASE WHEN s % 7 = 0 THEN NULL ELSE -(s % 999) * 0.01 END
  FROM generate_series(1, 200000) s;
VACUUM ANALYZE a220_n;
SELECT k, sum(p), sum(p * (1 - d)), sum(p * (1 - d) * (1 + d)), avg(p), avg(d), avg(p * d), count(*), count(nn), sum(nn), avg(nn) FROM a220_n GROUP BY k ORDER BY k;
SELECT sum(p * d) FROM a220_n WHERE d BETWEEN 0.02 AND 0.04 AND p < 5000;
SELECT k, sum(m), avg(m), count(m) FROM a220_n GROUP BY k ORDER BY k;
SELECT sum(m), avg(m) FROM a220_n WHERE m <> 'NaN' AND m < 'Infinity';
SELECT sum(i), avg(i), sum(i * p), avg(i * 1.5), sum(i - 0.5) FROM a220_n;
SELECT k, sum(nn), avg(nn), count(nn), count(*) FROM a220_n WHERE nn IS NULL GROUP BY k ORDER BY k;
SELECT sum(p), avg(p), count(*), count(p) FROM a220_n WHERE g < 0;
SELECT k, sum(p * 99999999999999.99 * 99999999999999.99), avg(p * 999999999999.99) FROM a220_n GROUP BY k ORDER BY k;
SELECT k, avg(p * (1 - d) * (1 + d)) FROM a220_n WHERE g % 3 = 1 GROUP BY k ORDER BY k;
SELECT sum(-p), sum(p - p), avg(p - p), sum(d * 0), avg(nn * 0), sum(0.000 * p) FROM a220_n;
SELECT k, i % 3 AS j, sum(p * d), avg(nn * d) FROM a220_n WHERE i > 400 GROUP BY k, j ORDER BY k, j;
DROP TABLE a220_n;
