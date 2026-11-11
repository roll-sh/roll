-- Merge Append over an inheritance tree in VERBOSE (its members computing the sort
-- expression, a Result above when the query returns other columns), a member's constant
-- sort key, a partitioned table pruned to one partition read in index order, a varchar
-- index column's condition without its relabeling, a one-item IN costed as its `=`.
CREATE TABLE gpl_m0 (id serial PRIMARY KEY, name text);
CREATE TABLE gpl_m1 (id integer PRIMARY KEY) INHERITS (gpl_m0);
CREATE TABLE gpl_m2 (id integer PRIMARY KEY) INHERITS (gpl_m0);
CREATE INDEX gpl_m0i ON gpl_m0 ((1 - id));
CREATE INDEX gpl_m1i ON gpl_m1 ((1 - id));
INSERT INTO gpl_m1 (name) VALUES ('a'), ('b');
INSERT INTO gpl_m2 (name) VALUES ('c'), ('d');
SET enable_seqscan = off;
SET enable_parallel_append = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_m0 ORDER BY 1 - id;
SELECT * FROM gpl_m0 ORDER BY 1 - id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(1 - id) FROM gpl_m0;
SELECT min(1 - id) FROM gpl_m0;
RESET enable_seqscan;
RESET enable_parallel_append;
CREATE TABLE gpl_u (thousand int, tenthous int, hundred int);
CREATE INDEX gpl_u_tt ON gpl_u (thousand, tenthous);
CREATE INDEX gpl_u_h ON gpl_u (hundred);
INSERT INTO gpl_u SELECT g % 1000, g, g % 100 FROM generate_series(1, 10000) g;
ANALYZE gpl_u;
EXPLAIN (COSTS OFF)
SELECT thousand, tenthous, thousand + tenthous AS x FROM gpl_u
UNION ALL
SELECT 42, 42, hundred FROM gpl_u
ORDER BY thousand, tenthous;
CREATE TABLE gpl_pm (a int, b varchar(16)) PARTITION BY RANGE (a);
CREATE TABLE gpl_pm1 PARTITION OF gpl_pm FOR VALUES FROM (1) TO (10);
CREATE TABLE gpl_pm2 PARTITION OF gpl_pm FOR VALUES FROM (10) TO (20);
CREATE INDEX gpl_pm1i ON gpl_pm1 (a, b);
CREATE INDEX gpl_pm2i ON gpl_pm2 (a, b);
INSERT INTO gpl_pm VALUES (1, '12345'), (2, '12345'), (15, 'x');
EXPLAIN (COSTS OFF) SELECT min(a), max(a) FROM gpl_pm WHERE b = '12345' AND a < 10;
SELECT min(a), max(a) FROM gpl_pm WHERE b = '12345' AND a < 10;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_pm WHERE a = 2 ORDER BY a, b;
RESET enable_seqscan;
RESET enable_bitmapscan;
CREATE TABLE gpl_v (a int, b varchar(16), c int);
CREATE INDEX gpl_v_b ON gpl_v (b);
INSERT INTO gpl_v SELECT g, g::text, g FROM generate_series(1, 1000) g;
VACUUM ANALYZE gpl_v;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_v WHERE b = '12';
EXPLAIN (COSTS OFF) SELECT b FROM gpl_v WHERE b = '12';
RESET enable_seqscan;
RESET enable_bitmapscan;
CREATE TABLE gpl_c (a int, b char(2));
EXPLAIN (COSTS OFF) SELECT * FROM gpl_c WHERE a BETWEEN 3 AND 23 AND b IN ('ab');
DROP TABLE gpl_m0 CASCADE;
DROP TABLE gpl_u, gpl_pm, gpl_c, gpl_v;
