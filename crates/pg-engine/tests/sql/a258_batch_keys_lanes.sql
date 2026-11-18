-- A parallel scan's batch aggregation against the rows PostgreSQL gives: group keys packed a
-- key column at a time (bpchar trimmed, NULLs, multibyte strings, keys too long to pack), numeric
-- lanes in 64 bits and past them (values, products and constants beyond an int8).
CREATE TABLE a258_k (id int, f char(1), s char(1), c char(10), v varchar(20), d date, b bool, q numeric(15,2), p numeric(15,2), t numeric(15,2), big numeric(30,2), k int) WITH (autovacuum_enabled = false);
INSERT INTO a258_k SELECT s,
  CASE WHEN s % 97 = 0 THEN NULL ELSE substr('ANR', 1 + s % 3, 1) END,
  CASE WHEN s % 89 = 0 THEN NULL ELSE substr('FO', 1 + s % 2, 1) END,
  CASE WHEN s % 83 = 0 THEN NULL ELSE (ARRAY['13-55', 'é1-x', '31', 'ab cd ef', 'zz', ' 1', '29-éé-00', '17'])[1 + s % 8] END,
  CASE WHEN s % 79 = 0 THEN NULL ELSE (ARRAY['ü', 'üü', 'abcdef', 'abcdefg', '', 'x y'])[1 + s % 6] END,
  CASE WHEN s % 73 = 0 THEN NULL ELSE date '1992-01-01' + (s % 2500) END,
  CASE WHEN s % 71 = 0 THEN NULL ELSE s % 2 = 0 END,
  CASE WHEN s % 67 = 0 THEN NULL ELSE ((s % 5000) - 1000) * 0.01 END,
  (s % 104950) + 900.5,
  (s % 11) * 0.01,
  CASE WHEN s % 5 = 0 THEN 123456789012345678.91 + s ELSE s * 1.5 END,
  s % 40
  FROM generate_series(1, 120000) s;
VACUUM ANALYZE a258_k;
SELECT f, s, count(*), sum(q), sum(p), sum(p * (1 - t)), sum(p * (1 - t) * (1 + t)), avg(q), avg(p), avg(t) FROM a258_k WHERE d <= date '1998-12-01' - interval '90' day GROUP BY f, s ORDER BY f, s;
SELECT c, count(*), sum(q) FROM a258_k GROUP BY c ORDER BY c;
SELECT v, count(*), sum(p) FROM a258_k WHERE q > 0 GROUP BY v ORDER BY v;
SELECT k, f, count(*), sum(t) FROM a258_k WHERE k < 5 GROUP BY k, f ORDER BY k, f;
SELECT k, d, count(*) FROM a258_k WHERE k = 3 AND d < date '1992-03-01' GROUP BY k, d ORDER BY k, d;
SELECT b, s, count(*), sum(q) FROM a258_k GROUP BY b, s ORDER BY b, s;
SELECT f, sum(big), avg(big), sum(big * t), sum(q * 100000000000000000000), sum(p * p * p * p) FROM a258_k GROUP BY f ORDER BY f;
SELECT s, sum(q * p * t), sum(-q - p), sum(q - 99999.99), sum(p * 1000000000000) FROM a258_k WHERE id > 1000 GROUP BY s ORDER BY s;
DROP TABLE a258_k;
