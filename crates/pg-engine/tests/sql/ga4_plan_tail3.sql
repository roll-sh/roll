-- Round 14 (agent 4), third batch: a relation's whole row in EXPLAIN (`i8.*`, null-extended
-- too), an IN list of its row type's constants as `= ANY (array)`, a byte-ordered COLLATE
-- on a catalog pattern still using the catalog's index, resjunk ORDER BY keys of an ordered
-- grouping under a LIMIT.
CREATE TABLE ga4_i8 (q1 int8, q2 int8);
INSERT INTO ga4_i8 VALUES (123, 456), (4567890123456789, 123), (1, 2), (NULL, 2);
EXPLAIN (COSTS OFF) SELECT * FROM ga4_i8 i8 WHERE i8 IN (row(123,456)::ga4_i8, '(4567890123456789,123)');
SELECT * FROM ga4_i8 i8 WHERE i8 IN (row(123,456)::ga4_i8, '(4567890123456789,123)') ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_i8 i8 WHERE i8 NOT IN (row(123,456)::ga4_i8, '(4567890123456789,123)');
SELECT * FROM ga4_i8 i8 WHERE i8 NOT IN (row(123,456)::ga4_i8, '(4567890123456789,123)') ORDER BY 1, 2;
SELECT i8 IN ('(1,2)', '(,2)') FROM ga4_i8 i8 ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT * FROM ga4_i8 i8 WHERE i8 = row(123,456)::ga4_i8;
EXPLAIN (VERBOSE, COSTS OFF) SELECT i8 FROM ga4_i8 i8;
CREATE TABLE ga4_t2 (a int, b int);
INSERT INTO ga4_t2 VALUES (1, NULL), (3, 3);
EXPLAIN (COSTS OFF) SELECT * FROM ga4_i8 LEFT JOIN ga4_t2 ON ga4_i8.q2 = ga4_t2.a WHERE ga4_t2 IS NOT NULL;
SELECT * FROM ga4_i8 LEFT JOIN ga4_t2 ON ga4_i8.q2 = ga4_t2.a WHERE ga4_t2 IS NULL ORDER BY 1, 2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (r).column2 FROM (SELECT r FROM (VALUES (1, 2), (3, 4)) r LIMIT 1) ss;

EXPLAIN (COSTS OFF) SELECT * FROM pg_class WHERE relname ~ '^pg_class$' COLLATE "POSIX";
EXPLAIN (COSTS OFF) SELECT * FROM pg_class WHERE relname LIKE 'pg\_class' COLLATE "POSIX";
SELECT relname FROM pg_class WHERE relname LIKE 'pg\_class' COLLATE "POSIX";

CREATE TABLE ga4_tk (unique1 int, thousand int, tenthous int);
INSERT INTO ga4_tk SELECT i, i % 1000, i FROM generate_series(0, 9999) i;
CREATE INDEX ga4_tk_thous_tenthous ON ga4_tk (thousand, tenthous);
VACUUM ANALYZE ga4_tk;
SET enable_hashagg = off;
SET enable_seqscan = off;
SET enable_sort = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT sum(tenthous) AS s1, sum(tenthous) + random() * 0 AS s2
  FROM ga4_tk GROUP BY thousand ORDER BY thousand LIMIT 3;
SELECT sum(tenthous) AS s1 FROM ga4_tk GROUP BY thousand ORDER BY thousand LIMIT 3;
RESET enable_hashagg;
RESET enable_seqscan;
RESET enable_sort;
