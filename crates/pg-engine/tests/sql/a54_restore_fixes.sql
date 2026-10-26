-- Round 19 (agent 54), what restoring PostgreSQL's dump of the regression database into the
-- engine failed on: functions over a shell type found by ALTER FUNCTION, a shell type's
-- ALTER TYPE ... OWNER, an identity column's sequence options LOGGED / UNLOGGED, a view over a
-- SQL function whose body doesn't bind under the current search_path (its body isn't read
-- until the call is planned), COPY into a table with no columns, COPY of regclass values with
-- an empty search_path (pg_catalog is always searched), a default operator class by binary
-- coercion (cidr's is inet's), a range constructor over a domain shown with the domain casts,
-- and ALTER FUNCTION <built-in> OWNER TO.
SET check_function_bodies = false;
SELECT pg_catalog.set_config('search_path', '', false);
CREATE SCHEMA a54s;
CREATE TYPE a54s.shelltype;
CREATE FUNCTION a54s.shelltype_in(cstring) RETURNS a54s.shelltype
    LANGUAGE internal IMMUTABLE STRICT AS $$textin$$;
CREATE FUNCTION a54s.shelltype_out(a54s.shelltype) RETURNS cstring
    LANGUAGE internal IMMUTABLE STRICT AS $$textout$$;
ALTER FUNCTION a54s.shelltype_in(cstring) OWNER TO CURRENT_USER;
ALTER FUNCTION a54s.shelltype_out(a54s.shelltype) OWNER TO CURRENT_USER;
COMMENT ON FUNCTION a54s.shelltype_out(a54s.shelltype) IS 'out';
CREATE TYPE a54s.shelltype (INTERNALLENGTH = variable, INPUT = a54s.shelltype_in,
    OUTPUT = a54s.shelltype_out, ALIGNMENT = int4, STORAGE = plain);
SELECT 'x'::text::a54s.shelltype;
CREATE TYPE a54s.othershell;
ALTER TYPE a54s.othershell OWNER TO CURRENT_USER;
GRANT USAGE ON TYPE a54s.othershell TO public;
-- Identity sequences' own persistence.
CREATE TABLE a54s.idl (a int NOT NULL);
ALTER TABLE a54s.idl ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME a54s.idl_a_seq UNLOGGED START WITH 1 INCREMENT BY 1 CACHE 1);
CREATE UNLOGGED TABLE a54s.idu (a int NOT NULL);
ALTER TABLE a54s.idu ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME a54s.idu_a_seq LOGGED);
CREATE TABLE a54s.idd (a int GENERATED ALWAYS AS IDENTITY (UNLOGGED LOGGED));
SELECT relname, relpersistence FROM pg_catalog.pg_class
  WHERE relnamespace = 'a54s'::regnamespace AND relname ~ '^id' ORDER BY 1;
-- A view over a SQL function whose body names a table off the path.
CREATE TABLE a54s.t (a int);
CREATE FUNCTION a54s.f(int) RETURNS int LANGUAGE sql AS $$select count(*)::int4 from t where a = $1$$;
CREATE VIEW a54s.v AS SELECT a, a54s.f(a) AS refcount FROM a54s.t;
SELECT * FROM a54s.v;
SELECT pg_catalog.pg_get_viewdef('a54s.v'::regclass);
SET search_path = a54s;
SELECT * FROM a54s.v;
SELECT pg_catalog.set_config('search_path', '', false);
-- COPY into no columns; regclass input with an empty path.
CREATE TABLE a54s.zero ();
COPY a54s.zero FROM stdin;


\.
SELECT count(*) FROM a54s.zero;
CREATE TABLE a54s.regs (r regclass, t regtype, p regproc, o regoper, n regnamespace);
COPY a54s.regs FROM stdin;
pg_class	regtype	now	||/	pg_catalog
\.
SELECT * FROM a54s.regs;
-- Default operator classes by binary coercion.
CREATE TABLE a54s.nets (c cidr, i inet);
CREATE INDEX nets_brin ON a54s.nets USING brin (i, c, c inet_minmax_ops);
CREATE INDEX nets_btree ON a54s.nets USING btree (c);
CREATE INDEX nets_hash ON a54s.nets USING hash (c);
SELECT indexrelid::regclass, indclass FROM pg_catalog.pg_index
  WHERE indrelid = 'a54s.nets'::regclass ORDER BY 1;
-- A range over a domain: its constructor's arguments are of the domain.
CREATE DOMAIN a54s.d1 AS int CHECK (VALUE < 100);
CREATE TYPE a54s.d1range AS RANGE (subtype = a54s.d1);
CREATE TABLE a54s.r (a int, b a54s.d1range GENERATED ALWAYS AS (a54s.d1range(a, a + 5)) STORED,
  c a54s.d1, e a54s.d1range DEFAULT a54s.d1range(1::a54s.d1, 2));
SELECT pg_catalog.pg_get_expr(adbin, adrelid) FROM pg_catalog.pg_attrdef
  WHERE adrelid = 'a54s.r'::regclass ORDER BY adnum;
CREATE VIEW a54s.rv AS SELECT a54s.d1range(c, c) FROM a54s.r;
SELECT pg_catalog.pg_get_viewdef('a54s.rv'::regclass);
INSERT INTO a54s.r (a) VALUES (1);
INSERT INTO a54s.r (a) VALUES (99);
SELECT a, b, e FROM a54s.r;
-- A built-in function's owner.
DROP ROLE IF EXISTS a54_fo;
CREATE ROLE a54_fo;
ALTER FUNCTION pg_catalog.pg_stat_reset OWNER TO a54_fo;
SELECT proowner::regrole, proacl FROM pg_catalog.pg_proc WHERE proname = 'pg_stat_reset';
REVOKE EXECUTE ON FUNCTION pg_catalog.pg_stat_reset FROM a54_fo;
SELECT proowner::regrole, proacl FROM pg_catalog.pg_proc WHERE proname = 'pg_stat_reset';
ALTER FUNCTION pg_catalog.lower(text) OWNER TO a54_fo;
SELECT proowner::regrole, proacl FROM pg_catalog.pg_proc WHERE oid = 'pg_catalog.lower(text)'::regprocedure;
ALTER AGGREGATE pg_catalog.count(*) OWNER TO a54_fo;
SELECT proowner::regrole FROM pg_catalog.pg_proc WHERE oid = 'pg_catalog.count()'::regprocedure;
ALTER FUNCTION pg_catalog.lower(text) OWNER TO a54_nope;
ALTER FUNCTION pg_catalog.lower(text) OWNER TO postgres;
ALTER AGGREGATE pg_catalog.count(*) OWNER TO postgres;
ALTER FUNCTION pg_catalog.pg_stat_reset OWNER TO postgres;
GRANT EXECUTE ON FUNCTION pg_catalog.pg_stat_reset() TO postgres;
SELECT proowner::regrole, proacl FROM pg_catalog.pg_proc WHERE proname = 'pg_stat_reset';
DROP ROLE a54_fo;
-- A NULL-constant default isn't stored (a domain's is); SET DEFAULT NULL removes one.
CREATE TABLE a54s.dn (a int DEFAULT NULL, b int DEFAULT 5, c int DEFAULT NULL::integer, d a54s.d1 DEFAULT NULL);
ALTER TABLE a54s.dn ALTER COLUMN b SET DEFAULT NULL;
ALTER TABLE a54s.dn ADD COLUMN e text DEFAULT NULL;
SELECT attname, pg_catalog.pg_get_expr(adbin, adrelid) FROM pg_catalog.pg_attrdef
  JOIN pg_catalog.pg_attribute ON attrelid = adrelid AND attnum = adnum
  WHERE adrelid = 'a54s.dn'::regclass ORDER BY 1;
-- Functions' parameter and result types lose their modifiers.
CREATE FUNCTION a54s.fc(character, varchar(5), numeric(4,2)) RETURNS character(3) LANGUAGE sql AS 'select $1';
SELECT pg_catalog.pg_get_function_identity_arguments('a54s.fc'::regproc), pg_catalog.pg_get_function_result('a54s.fc'::regproc);
SELECT a54s.fc('abcd', 'xxxxxxx', 123.456), pg_catalog.pg_typeof(a54s.fc('abc', 'x', 1.5));
DROP SCHEMA a54s CASCADE;
