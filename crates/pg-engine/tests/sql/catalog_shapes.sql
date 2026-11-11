-- The shape of the catalogs: built-in types, system relations' columns, and the oids and
-- attributes Postgres gives what users create.
SELECT oid, typname, typlen, typbyval, typalign, typstorage, typcategory, typispreferred, typdelim, typelem, typarray, typinput::oid, typoutput::oid, typreceive::oid, typsend::oid, typmodin::oid, typmodout::oid, typanalyze::oid, typsubscript::oid, typcollation, typtype, typnotnull, typndims, typtypmod, typbasetype, typrelid
FROM pg_type WHERE oid < 10000 AND typname NOT IN ('xid8', '_xid8', 'txid_snapshot', '_txid_snapshot', 'pg_snapshot', '_pg_snapshot', 'pg_ndistinct', 'pg_dependencies', 'pg_mcv_list', 'gtsvector', '_gtsvector', 'pg_brin_bloom_summary', 'pg_brin_minmax_multi_summary') ORDER BY oid;
SELECT c.relname, a.attnum, a.attname, a.atttypid::regtype, a.atttypmod, a.attcollation, a.attlen, a.attalign, a.attstorage, a.attnotnull, a.attbyval, a.attndims, a.attcompression, a.attislocal, a.attstattarget, a.attacl
FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
WHERE c.oid < 16384 AND c.relkind IN ('r', 'v') AND a.attnum > 0 AND c.relname NOT IN ('pg_statistic_ext_data', 'pg_stats_ext', 'pg_stats_ext_exprs') ORDER BY 1, 2;
CREATE TABLE cs_a (a int[], b int[][], c int ARRAY, d int[3][4], s serial);
CREATE TABLE cs_b AS SELECT a, ARRAY[1] AS x FROM cs_a;
CREATE VIEW cs_c AS SELECT a, ARRAY[1] AS x FROM cs_a;
CREATE MATERIALIZED VIEW cs_d AS SELECT a FROM cs_a;
CREATE TYPE cs_e AS (a int[], b text[][]);
CREATE TABLE cs_f (LIKE cs_a);
CREATE TABLE cs_g (e int[][][]) INHERITS (cs_a);
ALTER TABLE cs_a ADD COLUMN z text[][];
ALTER TABLE cs_a ALTER COLUMN c TYPE bigint[][][];
CREATE TABLE cs_t OF cs_e;
CREATE TABLE cs_p (a int[][]) PARTITION BY LIST (a);
CREATE TABLE cs_p1 PARTITION OF cs_p FOR VALUES IN ('{{1}}');
ALTER TYPE cs_e ADD ATTRIBUTE c int[][] CASCADE;
SELECT attrelid::regclass, attname, attndims FROM pg_attribute WHERE attrelid IN ('cs_a'::regclass, 'cs_b'::regclass, 'cs_c'::regclass, 'cs_d'::regclass, 'cs_f'::regclass, 'cs_g'::regclass, 'cs_t'::regclass, 'cs_p1'::regclass, (SELECT typrelid FROM pg_type WHERE typname = 'cs_e')) AND attnum > 0 ORDER BY 1, attnum;
SELECT t.oid::int8 - t.typrelid::int8, t.typarray::int8 - t.typrelid::int8 FROM pg_type t WHERE typname = 'cs_e';
ALTER TABLE cs_a ADD COLUMN m int DEFAULT 7;
SELECT attname, atthasmissing, attmissingval FROM pg_attribute WHERE attrelid = 'cs_a'::regclass AND attname = 'm';
DROP VIEW cs_c; DROP MATERIALIZED VIEW cs_d; DROP TABLE cs_b, cs_f, cs_g, cs_a, cs_t, cs_p; DROP TYPE cs_e;
CREATE TYPE cs_en AS ENUM ('a', 'b', 'c');
SELECT enumsortorder, enumlabel, oid::int8 % 2 AS odd FROM pg_enum WHERE enumtypid = 'cs_en'::regtype ORDER BY enumsortorder;
ALTER TYPE cs_en ADD VALUE 'ab' BEFORE 'b';
ALTER TYPE cs_en ADD VALUE 'z';
ALTER TYPE cs_en ADD VALUE '0' BEFORE 'a';
ALTER TYPE cs_en ADD VALUE 'aab' AFTER 'a';
ALTER TYPE cs_en ADD VALUE 'aaab' AFTER 'a';
SELECT enumsortorder, enumlabel FROM pg_enum WHERE enumtypid = 'cs_en'::regtype ORDER BY enumsortorder;
SELECT enum_range(NULL::cs_en), 'aab'::cs_en < 'ab'::cs_en, '0'::cs_en < 'a';
DROP TYPE cs_en;
