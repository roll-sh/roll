-- Round 17 (agent 37): what pg_dump's catalog queries need. "char" goes to text implicitly
-- (pg_cast), acldefault takes a "char", a name is a fixed-length array of "char"s, LOCK TABLE
-- takes system catalogs, SUPPORT and a C function's library and symbol are kept, a view
-- column's default is a pg_attrdef row, a publication may publish nothing, a view over a
-- catalog prints like any other, a domain keeps its COLLATE and stores its DEFAULT cooked,
-- a range keeps its SUBTYPE_OPCLASS, a composite type's column comment is pg_class's.
SELECT castsource::regtype, casttarget::regtype, castcontext FROM pg_cast
  WHERE castsource IN ('"char"'::regtype, 'regprocedure'::regtype, 'regconfig'::regtype)
  ORDER BY 1, 2;
SELECT relname FROM pg_class c WHERE c.relkind OPERATOR(pg_catalog.=) ANY (array['r', 'S'])
  AND relname = 'pg_class';
SELECT relname FROM pg_class WHERE relkind = 'r'::text AND relname = 'pg_type';
SELECT 'x'::"char" || 'y'::text;
SELECT acldefault('n', 10), acldefault('T'::"char", 10), acldefault('f', 10);
SELECT acldefault('z', 10);
SELECT typname[0], typname[1], typname[3], typname[63], typname[64], typname[-1]
  FROM pg_type WHERE oid = 23;
SELECT typname[0] = '_' FROM pg_type WHERE oid = 1007;
SELECT typname[1:2] FROM pg_type WHERE oid = 23;
CREATE TABLE a37_names (n name);
INSERT INTO a37_names VALUES ('abc');
UPDATE a37_names SET n[1] = 'X';
SELECT n FROM a37_names;
UPDATE a37_names SET n[0] = 'Z', n[1] = NULL;
SELECT n FROM a37_names;
DROP TABLE a37_names;
BEGIN;
LOCK TABLE pg_catalog.pg_class, pg_catalog.pg_type, pg_catalog.pg_default_acl IN ACCESS SHARE MODE;
LOCK TABLE pg_tables IN ACCESS SHARE MODE;
ROLLBACK;
BEGIN;
LOCK TABLE pg_catalog.pg_nosuch IN ACCESS SHARE MODE;
ROLLBACK;
-- SUPPORT, kept and printed.
CREATE FUNCTION a37_fws() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$ SUPPORT varchar_support;
CREATE FUNCTION a37_fws2() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$;
ALTER FUNCTION a37_fws2() SUPPORT varchar_support;
SELECT proname, prosupport FROM pg_proc WHERE proname LIKE 'a37_fws%' ORDER BY 1;
SELECT pg_get_functiondef('a37_fws'::regproc);
-- A C function: probin is the library, prosrc the symbol.
CREATE FUNCTION a37_plh() RETURNS language_handler
  AS '$libdir/plpgsql', 'plpgsql_call_handler' LANGUAGE c;
SELECT proname, l.lanname, prosrc, probin FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang
  WHERE proname = 'a37_plh';
SELECT pg_get_functiondef('a37_plh'::regproc);
DROP FUNCTION a37_fws(), a37_fws2(), a37_plh();
-- A view column's default.
CREATE VIEW a37_tv AS SELECT 1 AS col1, 2 AS col2;
ALTER VIEW a37_tv ALTER COLUMN col1 SET DEFAULT 1;
SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'a37_tv'::regclass;
SELECT attname, atthasdef FROM pg_attribute WHERE attrelid = 'a37_tv'::regclass ORDER BY attnum;
ALTER VIEW a37_tv ALTER COLUMN col1 DROP DEFAULT;
SELECT count(*) FROM pg_attrdef WHERE adrelid = 'a37_tv'::regclass;
DROP VIEW a37_tv;
-- A publication that publishes nothing.
SET client_min_messages = error;
CREATE PUBLICATION a37_pub FOR ALL TABLES WITH (publish = '');
CREATE PUBLICATION a37_pub2 FOR ALL TABLES WITH (publish = '  ');
RESET client_min_messages;
SELECT pubname, pubinsert, pubupdate, pubdelete, pubtruncate FROM pg_publication
  WHERE pubname LIKE 'a37_pub%' ORDER BY 1;
DROP PUBLICATION a37_pub, a37_pub2;
-- Views and materialized views over catalogs.
CREATE MATERIALIZED VIEW a37_mv AS SELECT count(*) FROM pg_class;
CREATE VIEW a37_v AS SELECT relname, relkind FROM pg_class c WHERE c.relkind = 'r';
SELECT pg_get_viewdef('a37_mv'::regclass);
SELECT pg_get_viewdef('a37_v'::regclass);
SELECT pg_get_viewdef('a37_v'::regclass, true);
DROP VIEW a37_v;
DROP MATERIALIZED VIEW a37_mv;
-- Domains: COLLATE, and the DEFAULT as stored.
CREATE DOMAIN a37_zip AS text COLLATE "C" DEFAULT '10014' CHECK (VALUE ~ '^\d{5}$') NOT NULL;
CREATE DOMAIN a37_num AS numeric(5,1) DEFAULT 1 + 1;
SELECT typname, typcollation::regcollation, typdefault,
  pg_get_expr(typdefaultbin, 'pg_type'::regclass) FROM pg_type
  WHERE typname IN ('a37_zip', 'a37_num') ORDER BY 1;
DROP DOMAIN a37_zip;
DROP DOMAIN a37_num;
-- A range's SUBTYPE_OPCLASS.
CREATE FUNCTION a37_int4eq(int, int) RETURNS bool LANGUAGE internal IMMUTABLE AS 'int4eq';
CREATE OPERATOR ~~~~ (FUNCTION = a37_int4eq, LEFTARG = int, RIGHTARG = int);
CREATE OPERATOR CLASS a37_opc FOR TYPE int USING btree AS OPERATOR 3 ~~~~(int, int);
CREATE TYPE a37_rng AS RANGE (subtype = int, subtype_opclass = a37_opc);
SELECT rngtypid::regtype, (SELECT opcname FROM pg_opclass WHERE oid = rngsubopc) FROM pg_range
  WHERE rngtypid = 'a37_rng'::regtype;
SELECT pg_describe_object(classid, objid, objsubid) AS obj,
  pg_describe_object(refclassid, refobjid, refobjsubid) AS ref, deptype
  FROM pg_depend WHERE objid = 'a37_rng'::regtype ORDER BY 2;
DROP TYPE a37_rng;
DROP OPERATOR CLASS a37_opc USING btree;
DROP OPERATOR FAMILY a37_opc USING btree;
DROP OPERATOR ~~~~ (int, int);
DROP FUNCTION a37_int4eq(int, int);
-- A composite type's column comment.
CREATE TYPE a37_comp AS (f1 int, f2 text);
COMMENT ON COLUMN a37_comp.f1 IS 'comment on column of type';
SELECT d.classoid::regclass, d.objsubid, d.objoid = t.typrelid AS same, d.description
  FROM pg_description d JOIN pg_type t ON t.typname = 'a37_comp' WHERE d.objoid = t.typrelid;
DROP TYPE a37_comp;
