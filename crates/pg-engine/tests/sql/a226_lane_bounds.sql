-- Sums and averages a parallel scan's workers add a batch at a time (batch_run) over
-- numbers near and past 64 bits: integers, bigints and numerics on both sides of 2^62,
-- their products and scaled sums, constants there, negative values, NULLs, an overflowing
-- integer product.
CREATE TABLE a226_b (g int, k int, b bigint, n numeric(20,2), m numeric(15,2)) WITH (autovacuum_enabled = false);
INSERT INTO a226_b SELECT s, s % 5,
  CASE s % 4 WHEN 0 THEN 4611686018427387903 WHEN 1 THEN -4611686018427387903 WHEN 2 THEN 4611686018427387904 ELSE s END,
  CASE WHEN s % 9 = 0 THEN NULL WHEN s % 2 = 0 THEN 46116860184273879.03 ELSE -(s * 1.01) END,
  (s % 100000) * 1.37
  FROM generate_series(1, 150000) s;
VACUUM ANALYZE a226_b;
SELECT k, sum(b), avg(b), count(b) FROM a226_b GROUP BY k ORDER BY k;
SELECT k, sum(n), avg(n), sum(n * 1), sum(n + m), sum(n - m) FROM a226_b GROUP BY k ORDER BY k;
SELECT k, sum(m * m), sum(m * m * m), sum(m * 4611686018427387903), sum(m * 4611686018427387904) FROM a226_b GROUP BY k ORDER BY k;
SELECT k, sum(g * 100000), sum(g * 4611686018427387903), avg(g + 0.01) FROM a226_b GROUP BY k ORDER BY k;
SELECT sum(m * 46116860184273.87), sum(m * 46116860184273.88), sum(-m * 46116860184273.88) FROM a226_b;
SELECT sum(b) FROM a226_b WHERE b > 0 AND b < 4611686018427387904;
DROP TABLE a226_b;
