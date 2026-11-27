-- sum / avg of numeric over a join (the serial aggregate) of computed numbers: products, sums
-- and differences of numerics and integers cast to numeric, NULLs, scales, values past an
-- i128 and the infinities; LIKE of literal patterns over stored strings.
CREATE TABLE a345_s (k int, g int, n int, b bigint);
CREATE TABLE a345_p (k int, x numeric(15,2), y numeric, z numeric(20,10));
INSERT INTO a345_s SELECT i, i % 3, CASE WHEN i % 7 = 0 THEN NULL ELSE i * 37 - 500 END, i::bigint * 1000000007 FROM generate_series(1, 60) i;
INSERT INTO a345_p SELECT i % 60 + 1, (i * 13 % 1000) / 7.0, CASE WHEN i % 11 = 0 THEN NULL ELSE i * 0.001 - 0.5 END, i / 3.0 FROM generate_series(1, 240) i;
INSERT INTO a345_p VALUES (5, 9999999999999.99, 123456789012345678901234567890.123, 1.5), (6, -9999999999999.99, -98765432109876543210987654321.98765, -2.25);
ANALYZE a345_s, a345_p;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SELECT s.g, sum(p.x * s.n), sum(p.x * p.y), avg(p.x * s.n), sum(p.y + s.n), sum(p.x - p.z), avg(p.z * 3), count(*)
FROM a345_s s JOIN a345_p p ON p.k = s.k GROUP BY s.g ORDER BY s.g;
-- an i128's range left: the big integer, a product of two large values, a scale that grows
SELECT s.g, sum(s.b * p.y), sum(p.y * p.y * p.y), sum(p.z * p.z * p.z * p.z), avg(s.b * p.x)
FROM a345_s s JOIN a345_p p ON p.k = s.k GROUP BY s.g ORDER BY s.g;
-- every row NULL, no rows, the infinities and NaN
SELECT sum(p.y * NULL::int), avg(p.x * s.n) FILTER (WHERE s.n IS NULL), sum(p.x * s.n) FILTER (WHERE false)
FROM a345_s s JOIN a345_p p ON p.k = s.k;
SELECT s.g, sum(p.x * v), avg(p.x + v) FROM a345_s s JOIN a345_p p ON p.k = s.k
CROSS JOIN (VALUES ('Infinity'::numeric), ('-Infinity'), ('NaN'), (1)) w(v) WHERE s.k <= 6 GROUP BY s.g ORDER BY s.g;
RESET enable_hashjoin;
RESET enable_mergejoin;
-- LIKE of literals and %, on stored strings (multibyte ones too) and NOT LIKE
CREATE TABLE a345_t (id int, s text, v varchar(20));
INSERT INTO a345_t SELECT i, (ARRAY['forest green', 'forestry', 'Forest', 'deep forest', 'éclair forest', 'forest é', 'for', NULL])[i % 8 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE ' ' || i END, 'PROMO' || i % 4 FROM generate_series(1, 400) i;
ANALYZE a345_t;
SELECT count(*) FILTER (WHERE s LIKE 'forest%'), count(*) FILTER (WHERE s LIKE '%forest'), count(*) FILTER (WHERE s LIKE '%est%re%'),
  count(*) FILTER (WHERE s NOT LIKE 'forest%'), count(*) FILTER (WHERE s LIKE 'é%'), count(*) FILTER (WHERE s LIKE '%é%3%')
FROM a345_t;
SELECT id, s FROM a345_t WHERE s LIKE 'forest%' AND id < 40 ORDER BY id;
SELECT id, s FROM a345_t WHERE s LIKE '%forest' AND v LIKE 'PROMO%' ORDER BY id LIMIT 10;
SELECT count(*) FROM a345_t WHERE s NOT LIKE '%r%es%';
DROP TABLE a345_s, a345_p, a345_t;
