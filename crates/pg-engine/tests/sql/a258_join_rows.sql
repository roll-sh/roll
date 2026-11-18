-- Joined rows that copy only what is read: a probe's or lookup's last match taking the outer
-- row's values, stacked hash joins, a nested loop's rows (its condition reading a column nothing
-- above reads), a left join's unmatched rows, against the rows PostgreSQL gives.
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
CREATE TABLE a258_dim (k int, name text, pad text, w numeric(15,2)) WITH (autovacuum_enabled = false);
INSERT INTO a258_dim SELECT s % 40, 'n' || s, repeat('p', 20 + s % 30), s * 0.25 FROM generate_series(1, 120) s;
CREATE INDEX ON a258_dim (k);
VACUUM ANALYZE a258_dim;
SELECT d.name, count(*), sum(x.q), min(x.c) FROM a258_k x JOIN a258_dim d ON d.k = x.k WHERE x.id % 7 = 0 GROUP BY d.name ORDER BY d.name LIMIT 20;
SELECT x.f, count(*), sum(d.w), sum(e.w) FROM a258_k x JOIN a258_dim d ON d.k = x.k JOIN a258_dim e ON e.k = (x.k + 1) % 40 WHERE x.id < 30000 GROUP BY x.f ORDER BY x.f;
SELECT x.id, x.c, d.name FROM a258_k x JOIN a258_dim d ON d.k = x.k AND d.w > x.t * 1000 WHERE x.id < 200 ORDER BY x.id, d.name LIMIT 40;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SELECT x.id, d.name FROM a258_k x JOIN a258_dim d ON d.k = x.k AND d.pad <> x.v WHERE x.id < 120 ORDER BY x.id, d.name LIMIT 30;
SELECT x.id, d.name, d.w FROM a258_k x LEFT JOIN a258_dim d ON d.k = x.k + 100 WHERE x.id < 10 ORDER BY x.id;
SELECT count(*), sum(length(d.pad)), sum(x.q) FROM a258_k x JOIN a258_dim d ON d.k = x.k WHERE x.id < 3000;
RESET enable_hashjoin;
RESET enable_mergejoin;
SELECT x.f, count(*), sum(x.q) FROM a258_k x WHERE x.d >= date '1993-01-01' AND x.d < timestamp '1994-01-01' AND EXISTS (SELECT 1 FROM a258_dim d WHERE d.k = x.k AND d.w > 10) GROUP BY x.f ORDER BY x.f;
SELECT x.s, count(d.name), sum(x.p) FROM a258_k x LEFT JOIN a258_dim d ON d.k = x.k + 1 AND d.w < 5 WHERE x.q > 30 AND x.id < 60000 GROUP BY x.s ORDER BY x.s;
SELECT x.f, count(*) FROM a258_k x WHERE x.q < 0 AND NOT EXISTS (SELECT 1 FROM a258_dim d WHERE d.k = x.k AND d.name LIKE 'n1%') GROUP BY x.f ORDER BY x.f;
DROP TABLE a258_dim;
DROP TABLE a258_k;
