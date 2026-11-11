-- VACUUM (VERBOSE), ANALYZE (VERBOSE) and VACUUM (VERBOSE, ANALYZE): heap_vacuum_rel's,
-- do_analyze_rel's and acquire_sample_rows' INFO reports, per table and per TOAST table, in
-- PostgreSQL's order. (The golden harness masks the numbers of transaction ids, rates,
-- buffers, WAL and time, and the oid in a TOAST table's name.) Agent 214.
CREATE TABLE vv_t (a int PRIMARY KEY, b text) WITH (autovacuum_enabled = off, toast.autovacuum_enabled = off);
INSERT INTO vv_t SELECT g, repeat('x', 10) FROM generate_series(1, 1000) g;
DELETE FROM vv_t WHERE a < 100;
VACUUM (VERBOSE) vv_t;
ANALYZE (VERBOSE) vv_t;
VACUUM (VERBOSE, ANALYZE) vv_t;
VACUUM (VERBOSE, FREEZE) vv_t;
VACUUM (VERBOSE, PROCESS_TOAST false) vv_t;
VACUUM (VERBOSE, PROCESS_MAIN false) vv_t;
-- No indexes: dead tuples' line pointers are unused at once.
CREATE TABLE vv_n (a int, b text) WITH (autovacuum_enabled = off, toast.autovacuum_enabled = off);
INSERT INTO vv_n SELECT g, 'x' FROM generate_series(1, 1000) g;
DELETE FROM vv_n WHERE a % 10 = 0;
VACUUM (VERBOSE) vv_n;
VACUUM (VERBOSE, FULL) vv_n;
VACUUM (VERBOSE) vv_n;
-- Few dead tuples in a big table: the index scan is bypassed.
CREATE TABLE vv_big (a int PRIMARY KEY, b text) WITH (autovacuum_enabled = off, toast.autovacuum_enabled = off);
INSERT INTO vv_big SELECT g, repeat('y', 50) FROM generate_series(1, 20000) g;
VACUUM vv_big;
DELETE FROM vv_big WHERE a = 5;
VACUUM (VERBOSE) vv_big;
-- Half the table deleted: index scan, truncation, deleted btree pages.
DELETE FROM vv_big WHERE a > 10000;
VACUUM (VERBOSE) vv_big;
VACUUM (VERBOSE, PROCESS_TOAST false) vv_big;
-- INDEX_CLEANUP off, then on.
DELETE FROM vv_big WHERE a < 50;
VACUUM (VERBOSE, INDEX_CLEANUP off, PROCESS_TOAST false) vv_big;
VACUUM (VERBOSE, INDEX_CLEANUP on, PROCESS_TOAST false) vv_big;
-- VACUUM FULL after deletes; CLUSTER; an aggressive VACUUM.
CREATE TABLE vv_x (a int PRIMARY KEY) WITH (autovacuum_enabled = off);
INSERT INTO vv_x SELECT generate_series(1, 500);
DELETE FROM vv_x WHERE a > 100;
VACUUM (VERBOSE, FULL) vv_x;
CLUSTER (VERBOSE) vv_x USING vv_x_pkey;
-- Partitions and inheritance.
CREATE TABLE vv_p (a int) PARTITION BY RANGE (a);
CREATE TABLE vv_p1 PARTITION OF vv_p FOR VALUES FROM (0) TO (100) WITH (autovacuum_enabled = off);
CREATE TABLE vv_p2 PARTITION OF vv_p FOR VALUES FROM (100) TO (200) WITH (autovacuum_enabled = off);
INSERT INTO vv_p SELECT g FROM generate_series(0, 199) g;
ANALYZE (VERBOSE) vv_p;
VACUUM (VERBOSE) vv_p;
CREATE TABLE vv_pe (a int) PARTITION BY LIST (a);
ANALYZE (VERBOSE) vv_pe;
CREATE TABLE vv_inh (a int) WITH (autovacuum_enabled = off);
CREATE TABLE vv_inhc () INHERITS (vv_inh) WITH (autovacuum_enabled = off);
INSERT INTO vv_inhc VALUES (1);
ANALYZE (VERBOSE) vv_inh;
DROP TABLE vv_inhc;
ANALYZE (VERBOSE) vv_inh;
ANALYZE (VERBOSE) vv_inh;
-- An empty table with no columns; a column list.
CREATE TABLE vv_e () WITH (autovacuum_enabled = off);
ANALYZE (VERBOSE) vv_e;
ANALYZE (VERBOSE) vv_big (a);
-- Other index access methods.
CREATE TABLE vv_g (a int, b int) WITH (autovacuum_enabled = off);
CREATE INDEX vv_g_brin ON vv_g USING brin (a);
CREATE INDEX vv_g_hash ON vv_g USING hash (b);
INSERT INTO vv_g SELECT g, g FROM generate_series(1, 100) g;
DELETE FROM vv_g WHERE a < 10;
VACUUM (VERBOSE) vv_g;
VACUUM (VERBOSE, INDEX_CLEANUP off) vv_g;
VACUUM (VERBOSE, DISABLE_PAGE_SKIPPING) vv_g;
CREATE TABLE vv_gs (p point) WITH (autovacuum_enabled = off);
CREATE INDEX vv_gs_gist ON vv_gs USING gist (p);
INSERT INTO vv_gs SELECT point(g, g) FROM generate_series(1, 100) g;
VACUUM (VERBOSE) vv_gs;
-- Two tables in one command; a skipped one.
VACUUM (VERBOSE, ANALYZE, SKIP_LOCKED) vv_n, vv_gs;
ANALYZE (VERBOSE) does_not_exist;
VACUUM (VERBOSE false) vv_n;
