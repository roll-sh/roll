-- x [NOT] IN (integer constants) as a set of integers (K::IntIn, VB::IntIn): int2, int4 and
-- int8 columns and items in any mix, NULL columns and NULL items, duplicates, lists past eight
-- items, and an IN sub-select's rows put in as a list under a parallel scan (TPC-H q16's
-- ps_suppkey NOT IN (SELECT s_suppkey ...)), against the rows PostgreSQL gives.
CREATE TABLE a261_i (id int, s int2, i int4, l int8, q numeric(12,2)) WITH (autovacuum_enabled = false);
INSERT INTO a261_i SELECT g,
  CASE WHEN g % 89 = 0 THEN NULL ELSE (g % 50 - 20)::int2 END,
  CASE WHEN g % 83 = 0 THEN NULL ELSE g % 1000 END,
  CASE WHEN g % 79 = 0 THEN NULL ELSE (g % 70) * 100000000000 END,
  (g % 997) * 0.25
  FROM generate_series(1, 60000) g;
CREATE TABLE a261_k (k int8, n int4);
INSERT INTO a261_k VALUES (3, 1), (17, 2), (17, 3), (250, 4), (999, 5), (4000000000, 6);
VACUUM ANALYZE a261_i;
VACUUM ANALYZE a261_k;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT count(*), sum(q) FROM a261_i WHERE i IN (1, 5, 9, 500, 999);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (1, 5, 9, 500, 999);
SELECT count(*), sum(q) FROM a261_i WHERE i IN (1, 5, NULL);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (1, 5, NULL);
SELECT count(*), sum(q) FROM a261_i WHERE s IN (-20, 0, 7::int8, 29, 29, 3);
SELECT count(*), sum(q) FROM a261_i WHERE s NOT IN (-20, 0, 7, 29, 100000::int4);
SELECT count(*), sum(q) FROM a261_i WHERE l IN (0, 100000000000, 6900000000000, 5::int2);
SELECT count(*), sum(q) FROM a261_i WHERE i IN (2, 4, 8, 16, 32, 64, 128, 256, 512, 3, 9, 27, 81, 243, 729, 4, 4);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (2, 4, 8, 16, 32, 64, 128, 256, 512, 3, 9, 27, 81, 243, 729);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (2, 4, 8, 16, 32, 64, 128, 256, 512, 3, NULL);
SELECT (i IN (1, 2, NULL)) AS t, count(*) FROM a261_i WHERE id <= 2000 GROUP BY 1 ORDER BY 1;
SELECT (i NOT IN (1, 2)) AS t, count(*) FROM a261_i WHERE id <= 2000 GROUP BY 1 ORDER BY 1;
SELECT count(*), sum(q) FROM a261_i WHERE i IN (SELECT k FROM a261_k);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (SELECT k FROM a261_k);
SELECT count(*), sum(q) FROM a261_i WHERE l NOT IN (SELECT n FROM a261_k);
INSERT INTO a261_k VALUES (NULL, NULL);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (SELECT k FROM a261_k);
SELECT count(*), sum(q) FROM a261_i WHERE i IN (SELECT k FROM a261_k);
SELECT count(*), sum(q) FROM a261_i WHERE i NOT IN (SELECT n FROM a261_k WHERE n > 2) AND q > 10;
DROP TABLE a261_i;
DROP TABLE a261_k;
