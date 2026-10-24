-- Round 17 (agent 37), more of what pg_dump reads: a view over a view deparses under
-- restrict_nonsystem_relation_kind (pg_dump sets it), arrays keep their element's type
-- modifier in the catalogs, partition bounds quote constants as get_const_expr does, WINDOW
-- and TRANSFORM FOR TYPE are kept, ALTER COLUMN SET COMPRESSION / SET (options) don't recurse,
-- system catalogs take column privileges (system columns too) and REVOKE from their initdb
-- ACL, COPY reads a system catalog, a range over a domain has the domain as rngsubtype, user
-- operators, base types and range constructors in stored expressions are qualified.
CREATE TABLE a37_base (x int, t text);
CREATE VIEW a37_v1 AS SELECT x, t FROM a37_base;
CREATE VIEW a37_v2 AS SELECT x FROM a37_v1 WHERE x > 0;
CREATE MATERIALIZED VIEW a37_mv AS SELECT t, count(*) FROM a37_v1 GROUP BY t;
SET restrict_nonsystem_relation_kind = 'view, foreign-table';
SELECT pg_get_viewdef('a37_v2'::regclass);
SELECT pg_get_viewdef('a37_mv'::regclass);
SELECT * FROM a37_v2;
RESET restrict_nonsystem_relation_kind;
DROP MATERIALIZED VIEW a37_mv;
DROP VIEW a37_v2, a37_v1;
DROP TABLE a37_base;
-- Array type modifiers.
CREATE TABLE a37_arr (f char(5)[], g varchar(5)[], n numeric(4,2)[][], ts timestamp(3)[]);
SELECT attname, atttypmod, format_type(atttypid, atttypmod), attndims FROM pg_attribute
  WHERE attrelid = 'a37_arr'::regclass AND attnum > 0 ORDER BY attnum;
CREATE DOMAIN a37_nums AS numeric(4,2)[];
SELECT typname, typtypmod, format_type(typbasetype, typtypmod) FROM pg_type
  WHERE typname = 'a37_nums';
SELECT format_type('_bpchar'::regtype, 9), format_type('_numeric'::regtype, 262150);
DROP DOMAIN a37_nums;
DROP TABLE a37_arr;
-- Partition bounds.
CREATE TABLE a37_pf (a float8) PARTITION BY RANGE (a);
CREATE TABLE a37_pf1 PARTITION OF a37_pf FOR VALUES FROM ('-Infinity') TO (0);
CREATE TABLE a37_pf2 PARTITION OF a37_pf FOR VALUES FROM (0) TO (10.5);
CREATE TABLE a37_pi (a int, b int8, c int2, d numeric) PARTITION BY RANGE (a, b, c, d);
CREATE TABLE a37_pi1 PARTITION OF a37_pi FOR VALUES FROM (-1, -2, -3, -4.5) TO (1, 2, 3, 4.5);
CREATE TABLE a37_pi2 PARTITION OF a37_pi FOR VALUES FROM (1, 2, 3, 4.5) TO (1, 2, 3, 5);
CREATE TABLE a37_pl (a int8) PARTITION BY LIST (a);
CREATE TABLE a37_pl1 PARTITION OF a37_pl FOR VALUES IN (1, -2);
SELECT relname, pg_get_expr(relpartbound, oid) FROM pg_class
  WHERE relname IN ('a37_pf1', 'a37_pf2', 'a37_pi1', 'a37_pi2', 'a37_pl1') ORDER BY 1;
DROP TABLE a37_pf, a37_pi, a37_pl;
-- WINDOW and TRANSFORM FOR TYPE.
CREATE FUNCTION a37_nth(anyelement, int) RETURNS anyelement LANGUAGE internal WINDOW
  IMMUTABLE STRICT AS 'window_nth_value';
SELECT prokind FROM pg_proc WHERE proname = 'a37_nth';
SELECT pg_get_functiondef('a37_nth'::regproc);
CREATE FUNCTION a37_trf(int) RETURNS int LANGUAGE sql AS 'select $1 + 1' TRANSFORM FOR TYPE int;
CREATE TRANSFORM FOR int LANGUAGE SQL (
    FROM SQL WITH FUNCTION prsd_lextype(internal),
    TO SQL WITH FUNCTION int4recv(internal));
CREATE FUNCTION a37_trf(int) RETURNS int LANGUAGE sql AS 'select $1 + 1' TRANSFORM FOR TYPE int;
SELECT protrftypes::regtype[] FROM pg_proc WHERE proname = 'a37_trf';
SELECT pg_get_functiondef('a37_trf'::regproc);
DROP FUNCTION a37_nth(anyelement, int), a37_trf(int);
DROP TRANSFORM FOR int LANGUAGE sql;
-- SET COMPRESSION and SET (options) don't recurse; SET STORAGE does.
CREATE TABLE a37_cp (f1 text COMPRESSION pglz);
CREATE TABLE a37_cc () INHERITS (a37_cp);
ALTER TABLE a37_cp ALTER COLUMN f1 SET COMPRESSION lz4;
ALTER TABLE a37_cp ALTER COLUMN f1 SET (n_distinct = 5);
ALTER TABLE a37_cp ALTER COLUMN f1 SET STORAGE external;
SELECT attrelid::regclass, attcompression, attoptions, attstorage FROM pg_attribute
  WHERE attrelid IN ('a37_cp'::regclass, 'a37_cc'::regclass) AND attname = 'f1' ORDER BY 1;
DROP TABLE a37_cp CASCADE;
-- System catalogs' privileges.
CREATE ROLE a37_role;
REVOKE SELECT ON TABLE pg_proc FROM public;
SELECT relacl FROM pg_class WHERE relname = 'pg_proc';
GRANT SELECT (tableoid, oid, proname) ON TABLE pg_proc TO public;
GRANT SELECT (ctid, prosrc) ON TABLE pg_proc TO a37_role;
SELECT attnum, attname, attacl FROM pg_attribute
  WHERE attrelid = 'pg_proc'::regclass AND attacl IS NOT NULL ORDER BY attnum;
GRANT SELECT (nosuch) ON TABLE pg_proc TO public;
REVOKE SELECT (tableoid, oid, proname) ON TABLE pg_proc FROM public;
REVOKE SELECT (ctid, prosrc) ON TABLE pg_proc FROM a37_role;
GRANT SELECT ON TABLE pg_proc TO public;
SELECT relacl FROM pg_class WHERE relname = 'pg_proc';
SELECT count(*) FROM pg_attribute WHERE attrelid = 'pg_proc'::regclass AND attacl IS NOT NULL;
DROP ROLE a37_role;
-- COPY from a system catalog.
COPY pg_catalog.pg_am (amname, amtype) TO stdout;
COPY (SELECT amname FROM pg_am ORDER BY 1 LIMIT 2) TO stdout;
COPY pg_catalog.pg_am (nosuch) TO stdout;
COPY pg_catalog.pg_tables TO stdout;
-- A range over a domain; user operators and base types in stored expressions.
CREATE DOMAIN a37_dom AS int CHECK (VALUE < 100);
CREATE TYPE a37_dr AS RANGE (subtype = a37_dom);
SELECT rngsubtype::regtype FROM pg_range WHERE rngtypid = 'a37_dr'::regtype;
CREATE FUNCTION a37_in(cstring) RETURNS a37_al STRICT IMMUTABLE LANGUAGE internal AS 'int8in';
CREATE FUNCTION a37_out(a37_al) RETURNS cstring STRICT IMMUTABLE LANGUAGE internal AS 'int8out';
CREATE TYPE a37_al (input = a37_in, output = a37_out, like = int8);
CREATE FUNCTION a37_lt(a37_al, a37_al) RETURNS bool STRICT IMMUTABLE LANGUAGE internal AS 'int8lt';
CREATE OPERATOR < (procedure = a37_lt, leftarg = a37_al, rightarg = a37_al);
CREATE TABLE a37_ec (f1 a37_al, a int, b a37_dr GENERATED ALWAYS AS (a37_dr(a, a + 5)) STORED);
ALTER TABLE a37_ec ENABLE ROW LEVEL SECURITY;
CREATE POLICY p1 ON a37_ec USING (f1 < '5'::a37_al);
SET search_path = '';
SELECT pg_get_expr(polqual, polrelid) FROM pg_policy WHERE polrelid = 'public.a37_ec'::regclass;
RESET search_path;
DROP TABLE a37_ec;
DROP TYPE a37_dr;
DROP DOMAIN a37_dom;
DROP TYPE a37_al CASCADE;
-- A whole-row CHECK depends on the table alone (pg_dump keeps it inline).
create table a37w (a int, b int, constraint c1 check (a > b), constraint c2 check ((a37w.*) is not null), constraint c3 check (1 > 0));
select pg_describe_object(classid, objid, objsubid) obj, pg_describe_object(refclassid, refobjid, refobjsubid) ref, deptype from pg_depend where objid in (select oid from pg_constraint where conrelid = 'a37w'::regclass and contype = 'c') order by 1, 2, 3;
drop table a37w;
-- A serial's sequence kept when the column became an identity is only owned by it.
CREATE TABLE a37itest14 (id serial);
ALTER TABLE a37itest14 ALTER id DROP DEFAULT;
ALTER TABLE a37itest14 ALTER id ADD GENERATED BY DEFAULT AS IDENTITY;
select pg_describe_object(classid, objid, objsubid) obj, pg_describe_object(refclassid, refobjid, refobjsubid) ref, deptype from pg_depend where refobjid = 'a37itest14'::regclass and classid = 'pg_class'::regclass order by 1;
select pg_get_serial_sequence('a37itest14', 'id');
drop table a37itest14;
