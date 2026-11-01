CREATE EXTENSION postgres_fdw;
CREATE SERVER remote FOREIGN DATA WRAPPER postgres_fdw OPTIONS (fdw_tuple_cost '0', fdw_startup_cost '10000');
CREATE USER MAPPING FOR CURRENT_USER SERVER remote;
CREATE TABLE l (i int PRIMARY KEY, v text);
INSERT INTO l SELECT i, 'local' FROM generate_series(1, 1000) i;
ANALYZE l;
CREATE FOREIGN TABLE f1 (i int, v text) SERVER remote OPTIONS (table_name 't1');
CREATE FOREIGN TABLE f2 (i int, v text) SERVER remote OPTIONS (table_name 't2');
SET enable_nestloop = off;
SET enable_mergejoin = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT l.i, f1.i, f1.v, f2.i FROM l, f1, f2
WHERE f1.i = f2.i AND l.i = f1.i AND f1.i < 30 AND f2.i < 30 FOR UPDATE OF l;
