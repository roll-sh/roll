-- A GroupAggregate over an index scan in its keys' order streams its groups (one held at
-- a time): rows, order, LIMIT, HAVING, permuted and redundant keys, DESC columns, errors
-- at a later group.
CREATE TABLE a101_g (k int, j int, v numeric, t text, PRIMARY KEY (k, j));
INSERT INTO a101_g SELECT i / 3, i % 3, i * 1.5, 't' || (i % 7) FROM generate_series(1, 3000) i;
ANALYZE a101_g;
SET enable_hashagg = off;
SET enable_sort = off;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT k, sum(v), count(*) FROM a101_g GROUP BY k;
SELECT k, sum(v), count(*), min(t), max(v) FROM a101_g GROUP BY k LIMIT 6;
SELECT k, sum(v) FROM a101_g WHERE k BETWEEN 400 AND 405 GROUP BY k;
SELECT k, count(*) FROM a101_g GROUP BY k HAVING sum(v) > 4490 LIMIT 4;
EXPLAIN (COSTS OFF) SELECT j, k, sum(v) FROM a101_g GROUP BY j, k;
SELECT j, k, sum(v) FROM a101_g GROUP BY j, k LIMIT 5;
SELECT count(*), sum(s) FROM (SELECT k, sum(v) s FROM a101_g GROUP BY k) x;
-- an error in a later group's aggregate argument
SELECT k, sum(10 / (k - 700)) FROM a101_g GROUP BY k;
SELECT k, sum(10 / (k - 700)) FROM a101_g GROUP BY k LIMIT 3;
-- NULL keys come together, last
CREATE TABLE a101_n (a int, b int);
CREATE INDEX a101_n_a ON a101_n (a);
INSERT INTO a101_n SELECT CASE WHEN i % 4 = 0 THEN NULL ELSE i % 5 END, i FROM generate_series(1, 200) i;
ANALYZE a101_n;
EXPLAIN (COSTS OFF) SELECT a, count(*), sum(b) FROM a101_n GROUP BY a;
SELECT a, count(*), sum(b) FROM a101_n GROUP BY a;
RESET enable_hashagg;
RESET enable_sort;
RESET enable_seqscan;
DROP TABLE a101_g, a101_n;
