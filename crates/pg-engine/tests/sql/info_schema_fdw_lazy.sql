-- information_schema columns computed through pg_relation_is_updatable / pg_column_is_updatable
-- ask a foreign table's wrapper, which fails without a handler: only for the rows returned,
-- and only when the column is read.
CREATE FOREIGN DATA WRAPPER isfl_fdw;
CREATE SERVER isfl_srv FOREIGN DATA WRAPPER isfl_fdw;
CREATE FOREIGN TABLE isfl_ft (a int) SERVER isfl_srv;
CREATE TABLE isfl_t (a int);
CREATE VIEW isfl_v AS SELECT a FROM isfl_t;
SELECT table_name, is_insertable_into FROM information_schema.tables WHERE table_name = 'isfl_t';
SELECT table_name, is_updatable, is_insertable_into FROM information_schema.views WHERE table_name = 'isfl_v';
SELECT table_name, column_name, is_updatable FROM information_schema.columns WHERE table_name IN ('isfl_t', 'isfl_v') ORDER BY 1;
SELECT count(*) FROM information_schema.tables WHERE table_name LIKE 'isfl%';
SELECT table_name, table_type FROM information_schema.tables WHERE table_name LIKE 'isfl%' ORDER BY 1;
SELECT table_name, column_name FROM information_schema.columns WHERE table_name LIKE 'isfl%' ORDER BY 1;
SELECT table_name, is_insertable_into FROM information_schema.tables WHERE table_name LIKE 'isfl%' ORDER BY 1;
SELECT * FROM information_schema.tables WHERE table_name LIKE 'isfl%' ORDER BY 1;
SELECT table_name FROM information_schema.tables WHERE is_insertable_into = 'NO' AND table_name LIKE 'isfl%';
SELECT table_name, is_updatable FROM information_schema.columns WHERE table_name LIKE 'isfl%' ORDER BY 1;
SELECT pg_relation_is_updatable('isfl_ft'::regclass, false);
SELECT pg_column_is_updatable('isfl_ft'::regclass, 1::int2, false);
SELECT pg_relation_is_updatable('isfl_t'::regclass, false), pg_relation_is_updatable('isfl_v'::regclass, false);
DROP FOREIGN TABLE isfl_ft;
SELECT table_name, is_insertable_into FROM information_schema.tables WHERE table_name LIKE 'isfl%' ORDER BY 1;
DROP VIEW isfl_v;
DROP TABLE isfl_t;
DROP SERVER isfl_srv;
DROP FOREIGN DATA WRAPPER isfl_fdw;
