-- Btree skip scans (PostgreSQL 18): a condition on a later key column with none (or a
-- range) on the ones before reads each distinct leading value's entries (agent 114).
-- NULL and DESC leading columns, text keys, ranges and `= ANY` on the later column,
-- backward scans, LIMIT, own writes, many and few distinct values.
CREATE TABLE a114_s (a int, b int, c text, d int);
INSERT INTO a114_s SELECT g % 5, g / 5, 'k' || (g % 3), g FROM generate_series(1, 2000) g;
INSERT INTO a114_s VALUES (NULL, 7, NULL, -1), (NULL, 400, 'k0', -2), (9, NULL, 'k9', -3);
CREATE INDEX a114_s_ab ON a114_s (a, b);
CREATE INDEX a114_s_cdb ON a114_s (c, d DESC, b);
CREATE INDEX a114_s_adb ON a114_s (a DESC NULLS LAST, b);
ANALYZE a114_s;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT a, b, d FROM a114_s WHERE b = 7 ORDER BY a, b;
SELECT a, b, d FROM a114_s WHERE b = 7 ORDER BY a, b;
SELECT a, b FROM a114_s WHERE b = 7 ORDER BY a DESC, b DESC;
SELECT a, b, d FROM a114_s WHERE b >= 399 AND b < 401 ORDER BY a, b;
SELECT a, b FROM a114_s WHERE b > 398 ORDER BY a, b LIMIT 3;
SELECT a, b FROM a114_s WHERE b = ANY ('{3, 250, 1000}') ORDER BY a, b;
SELECT a, b FROM a114_s WHERE a > 2 AND b = 100 ORDER BY a, b;
SELECT a, b FROM a114_s WHERE a < 3 AND b <= 1 ORDER BY a, b;
SELECT a, b FROM a114_s WHERE a IS NULL AND b = 7;
SELECT a, b FROM a114_s WHERE b IS NULL;
SELECT count(*) FROM a114_s WHERE b = 9999;
EXPLAIN (COSTS OFF) SELECT c, d, b FROM a114_s WHERE b = 7 ORDER BY c, d DESC, b;
SELECT c, d, b FROM a114_s WHERE b = 7 ORDER BY c, d DESC, b;
SELECT c, d, b FROM a114_s WHERE d = 1500 ORDER BY c, d DESC;
SELECT c, d, b FROM a114_s WHERE c > 'k0' AND d < 10 ORDER BY c DESC, d;
SELECT a, b FROM a114_s WHERE b = 7 ORDER BY a DESC NULLS LAST, b;
BEGIN;
INSERT INTO a114_s VALUES (2, 7, 'k1', 5001), (NULL, 7, 'k5', 5002), (50, 7, NULL, 5003);
DELETE FROM a114_s WHERE d = 35;
SELECT a, b, d FROM a114_s WHERE b = 7 ORDER BY a, b, d;
SELECT c, d FROM a114_s WHERE d = 5003 OR d = 5001 ORDER BY c, d DESC;
ROLLBACK;
-- Many distinct leading values: each read through or sought.
CREATE TABLE a114_s2 (a int, b int);
INSERT INTO a114_s2 SELECT g, g % 7 FROM generate_series(1, 3000) g;
INSERT INTO a114_s2 SELECT g % 4, g FROM generate_series(1, 3000) g;
CREATE INDEX ON a114_s2 (a, b);
ANALYZE a114_s2;
SELECT count(*), sum(a) FROM a114_s2 WHERE b = 3;
SELECT a, b FROM a114_s2 WHERE b = 2999 ORDER BY a DESC, b DESC;
SELECT count(*) FROM a114_s2 WHERE b BETWEEN 5 AND 6 AND a > 2990;
-- A composite primary key's skip scan.
CREATE TABLE a114_s3 (t int, id int, v text, PRIMARY KEY (t, id));
INSERT INTO a114_s3 SELECT g % 3, g, 'v' || g FROM generate_series(1, 900) g;
ANALYZE a114_s3;
SELECT t, id, v FROM a114_s3 WHERE id = 500 OR id = 501 ORDER BY t, id;
SELECT t, id FROM a114_s3 WHERE id > 897 ORDER BY t, id;
SELECT t, id FROM a114_s3 WHERE id < 4 ORDER BY t DESC, id DESC;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a114_s, a114_s2, a114_s3;
