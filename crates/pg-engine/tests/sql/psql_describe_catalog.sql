-- Catalog functions and columns psql's describe commands (describe.c) read.
-- \dp: an ACL with no entries is a one-dimensional empty array, shown as (none).
CREATE TABLE pdc_acl (a int);
REVOKE ALL ON pdc_acl FROM postgres;
SELECT relacl, cardinality(relacl), array_length(relacl, 1), relacl = '{}' FROM pg_class WHERE relname = 'pdc_acl';
SELECT CASE WHEN pg_catalog.array_length(c.relacl, 1) = 0 THEN '(none)' ELSE pg_catalog.array_to_string(c.relacl, E'\n') END AS "Access privileges" FROM pg_class c WHERE relname = 'pdc_acl';
-- \df: a procedure has no result; one OUT parameter is the result type.
CREATE PROCEDURE pdc_proc(x text) LANGUAGE sql AS $$ SELECT 1 $$;
SELECT pg_get_function_result('pdc_proc'::regproc) IS NULL;
CREATE FUNCTION pdc_out1(a int DEFAULT 1, OUT sum int, b int DEFAULT 2) LANGUAGE sql AS $$ SELECT $1 + $2 $$;
SELECT pg_get_function_result('pdc_out1'::regproc), pg_get_function_arguments('pdc_out1'::regproc);
CREATE FUNCTION pdc_out2(a int, OUT x int, OUT y text) LANGUAGE sql AS $$ SELECT $1, 'a' $$;
SELECT pg_get_function_result('pdc_out2'::regproc);
CREATE FUNCTION pdc_setof(a int, OUT x int) RETURNS SETOF int LANGUAGE sql AS $$ SELECT $1 $$;
SELECT pg_get_function_result('pdc_setof'::regproc);
CREATE FUNCTION pdc_table(a int) RETURNS TABLE (x int, y text) LANGUAGE sql AS $$ SELECT $1, 'a' $$;
SELECT pg_get_function_result('pdc_table'::regproc);
-- regclass: a user table pg_catalog's relation of the same name hides is qualified.
CREATE TABLE pg_attrdef (a int);
SELECT 'public.pg_attrdef'::regclass, 'pg_attrdef'::regclass::oid < 16384;
SELECT relname FROM pg_class WHERE oid = 'public.pg_attrdef'::regclass;
DROP TABLE public.pg_attrdef;
-- pg_get_constraintdef: the referenced table as the search path sees it.
CREATE SCHEMA pdc_s;
SET search_path = pdc_s;
CREATE TABLE pk (x int PRIMARY KEY);
CREATE TABLE fk (b int REFERENCES pk);
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'fk_b_fkey';
RESET search_path;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'fk_b_fkey';
-- Partition bounds in the session's DateStyle; array keys compare with OR.
CREATE TABLE pdc_d (d date) PARTITION BY RANGE (d);
CREATE TABLE pdc_d1 PARTITION OF pdc_d FOR VALUES FROM ('2016-07-01') TO ('2016-08-01');
SET datestyle = 'Postgres, MDY';
SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'pdc_d1';
RESET datestyle;
SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'pdc_d1';
CREATE TABLE pdc_arr (a int[]) PARTITION BY LIST (a);
CREATE TABLE pdc_arr1 PARTITION OF pdc_arr FOR VALUES IN ('{1}', '{2}');
CREATE TABLE pdc_arr2 PARTITION OF pdc_arr FOR VALUES IN ('{3}');
SELECT pg_get_partition_constraintdef('pdc_arr1'::regclass);
SELECT pg_get_partition_constraintdef('pdc_arr2'::regclass);
-- \d+ index: an index column's type is its operator class's key type, an expression's
-- its type; a column's storage carries over.
CREATE TABLE pdc_x (a int, d float8, e int, n name, t text, i int[], v tsvector);
CREATE INDEX pdc_xi ON pdc_x (a, (d + e), n);
CREATE INDEX pdc_gin ON pdc_x USING gin (i);
ALTER TABLE pdc_x ALTER COLUMN t SET STORAGE external;
CREATE INDEX pdc_xt ON pdc_x (t);
SELECT attrelid::regclass, attname, atttypid::regtype, attstorage FROM pg_attribute WHERE attrelid IN ('pdc_xi'::regclass, 'pdc_gin'::regclass, 'pdc_xt'::regclass) ORDER BY attrelid::regclass::text, attnum;
-- Operator class options.
CREATE INDEX pdc_v1 ON pdc_x USING gist (v tsvector_ops (siglen = 100));
CREATE INDEX pdc_v2 ON pdc_x USING gist (v tsvector_ops (siglen = 0));
CREATE INDEX pdc_v3 ON pdc_x USING gist (v tsvector_ops (siglen = 100, foo = 'bar'));
CREATE INDEX pdc_v4 ON pdc_x USING gist (v tsvector_ops (siglen = 1, siglen = 2));
CREATE INDEX pdc_v5 ON pdc_x USING gist (v tsvector_ops (siglen = abc));
CREATE INDEX pdc_a1 ON pdc_x (a int4_ops (foo = 1));
CREATE INDEX pdc_b1 ON pdc_x USING brin (a int4_minmax_multi_ops (values_per_range = 4));
CREATE INDEX pdc_b2 ON pdc_x USING brin (a int4_bloom_ops (n_distinct_per_range = 10, false_positive_rate = 0.01));
CREATE INDEX pdc_b3 ON pdc_x USING brin (a int4_bloom_ops (false_positive_rate = 0.9));
SELECT pg_get_indexdef('pdc_v1'::regclass), pg_get_indexdef('pdc_b2'::regclass);
SELECT attoptions FROM pg_attribute WHERE attrelid = 'pdc_v1'::regclass;
-- System catalog and TOAST indexes.
SELECT pg_get_indexdef('pg_class_oid_index'::regclass), pg_get_indexdef('pg_class_oid_index'::regclass, 0, true);
SELECT pg_get_indexdef('pg_toast.pg_toast_2619_index'::regclass), pg_get_indexdef('pg_toast.pg_toast_2619_index'::regclass, 2, true);
-- A serial or identity column's sequence is as persistent as its table.
CREATE UNLOGGED TABLE pdc_ul (a int GENERATED ALWAYS AS IDENTITY, b serial);
SELECT relname, relpersistence FROM pg_class WHERE relname LIKE 'pdc_ul%' ORDER BY 1;
-- pg_*_is_visible.
CREATE TYPE pdc_ct AS (a int, b text);
SELECT relkind, pg_table_is_visible(oid) FROM pg_class WHERE relname = 'pdc_ct';
CREATE STATISTICS pdc_st ON a, d FROM pdc_x;
SELECT stxname, pg_statistics_obj_is_visible(oid) FROM pg_statistic_ext WHERE stxname = 'pdc_st';
CREATE OPERATOR ### (leftarg = int, rightarg = int, function = int4pl);
CREATE OPERATOR pdc_s.### (leftarg = int, rightarg = int, function = int4mi);
CREATE TYPE pdc_e AS ENUM ('a');
CREATE TYPE pdc_s.pdc_e AS ENUM ('b');
CREATE COLLATION pdc_coll (locale = 'C');
CREATE OPERATOR FAMILY pdc_fam USING btree;
SET search_path = pdc_s, public;
SELECT oprnamespace::regnamespace, pg_operator_is_visible(oid) FROM pg_operator WHERE oprname = '###' ORDER BY 1;
SELECT typnamespace::regnamespace, pg_type_is_visible(oid) FROM pg_type WHERE typname = 'pdc_e' ORDER BY 1;
SELECT collname, pg_collation_is_visible(oid) FROM pg_collation WHERE collname = 'pdc_coll';
SELECT opfname, pg_opfamily_is_visible(oid) FROM pg_opfamily WHERE opfname = 'pdc_fam';
SELECT pg_operator_is_visible(oid) FROM pg_operator WHERE oprname = '+' AND oprleft = 'int4'::regtype AND oprright = 'int4'::regtype;
RESET search_path;
SELECT typnamespace::regnamespace, pg_type_is_visible(oid) FROM pg_type WHERE typname = 'pdc_e' ORDER BY 1;
-- \dP: an index attached to a partitioned index is a partition.
CREATE TABLE pdc_ip (a int) PARTITION BY RANGE (a);
CREATE TABLE pdc_ip1 PARTITION OF pdc_ip FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (a);
CREATE TABLE pdc_ip11 PARTITION OF pdc_ip1 FOR VALUES FROM (0) TO (5);
CREATE INDEX pdc_ipi ON pdc_ip (a);
SELECT relname, relkind, relispartition FROM pg_class WHERE relname LIKE 'pdc_ip%' ORDER BY 1;
SELECT inhrelid::regclass, inhparent::regclass FROM pg_inherits WHERE inhparent = 'pdc_ipi'::regclass;
-- \sf of built-in functions.
SELECT pg_get_functiondef('abs(int)'::regprocedure);
SELECT pg_get_functiondef('interval_pl_time'::regproc);
SELECT 'information_schema._pg_index_position'::regproc;
SELECT pg_get_functiondef('information_schema._pg_index_position(oid, smallint)'::regprocedure);
SELECT pg_get_functiondef('sum(int)'::regprocedure);
