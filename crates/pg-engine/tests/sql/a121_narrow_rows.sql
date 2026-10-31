-- Rows held narrowed: hash join inner rows and full sorts keep only the columns read
-- (agent 121). Wide rows, few columns read; outer joins' unmatched inner rows,
-- expression keys, sorts with and without LIMIT, nested loops' inner rows.
CREATE TABLE a121_w (id int PRIMARY KEY, g int, t1 text, t2 text, n numeric, d date, f float8, b bool, t3 text);
INSERT INTO a121_w SELECT i, i % 37, 'one ' || i, repeat('x', i % 50), i / 7.0, date '2020-01-01' + i % 400, i * 1.5, i % 3 = 0, 'three ' || (i % 11)
  FROM generate_series(1, 3000) i;
CREATE TABLE a121_v (k int, g int, s text, u text, m numeric, w text, z int);
INSERT INTO a121_v SELECT i, i % 41, 'vs ' || i, repeat('y', i % 30), i * 0.25, 'w' || (i % 5), i % 7
  FROM generate_series(1, 4000) i;
CREATE INDEX a121_v_g ON a121_v (g);
ANALYZE a121_w;
ANALYZE a121_v;
SET enable_mergejoin = off;
SET enable_nestloop = off;
-- hash joins: the inner side wide, few columns read
SELECT a.id, b.s FROM a121_w a JOIN a121_v b ON b.k = a.id WHERE a.id % 500 = 0 ORDER BY a.id;
SELECT count(*), sum(b.m), max(b.s) FROM a121_w a JOIN a121_v b ON b.g = a.g;
SELECT a.g, count(*), sum(a.n) FROM a121_v b JOIN a121_w a ON a.id = b.k GROUP BY a.g ORDER BY a.g LIMIT 5;
-- an expression key
SELECT a.id, b.k FROM a121_w a JOIN a121_v b ON lower(b.s) = 'vs ' || a.id WHERE a.id < 6 ORDER BY 1;
-- right and full joins: the inner rows nothing matched come out with their columns
SELECT a.id, b.k, b.w FROM a121_w a RIGHT JOIN a121_v b ON b.k = a.id WHERE b.k > 2995 ORDER BY b.k LIMIT 8;
SELECT a.id, b.k, b.u FROM (SELECT * FROM a121_w WHERE id > 2990) a FULL JOIN (SELECT * FROM a121_v WHERE k BETWEEN 2985 AND 2995) b ON b.k = a.id ORDER BY a.id, b.k;
-- sorts of wide rows: a few columns read above
SELECT id, t3 FROM (SELECT * FROM a121_w ORDER BY t3 DESC, id) s WHERE id % 300 = 1;
SELECT g, string_agg(t1, ',' ORDER BY id) FROM (SELECT * FROM a121_w ORDER BY g, id) s WHERE id < 100 GROUP BY g ORDER BY g LIMIT 4;
SELECT a.id, b.k, a.t1 FROM a121_w a JOIN a121_v b ON b.k = a.id ORDER BY a.f DESC, b.z LIMIT 5;
SELECT a.id, b.k, a.d FROM a121_w a JOIN a121_v b ON b.k = a.id ORDER BY a.d, a.id DESC OFFSET 2990;
-- nested loops: the inner side read with the columns used
RESET enable_nestloop;
SET enable_hashjoin = off;
SELECT a.id, b.s FROM a121_w a JOIN a121_v b ON b.g = a.g WHERE a.id IN (1, 2) ORDER BY 1, 2 LIMIT 6;
SELECT a.id, (SELECT count(*) FROM a121_v b WHERE b.g = a.g AND b.z = 3) FROM a121_w a WHERE a.id < 4 ORDER BY 1;
RESET enable_hashjoin;
RESET enable_mergejoin;
DROP TABLE a121_w, a121_v;
