-- Client suites round 6 (agent 122): Doctrine DBAL renumbering a table onto another object's
-- oid and back; a FROM subquery's whole row passed to a function from a sub-select (Hasura's
-- computed fields), and coerce_record_to_complex's errors; NULL defaults of columns with a
-- type modifier (Metabase's Liquibase changelog).

-- DBAL's testListTableColumnsOidConflictWithNonTableObject.
CREATE TABLE test_oidc (id int NOT NULL, note text);
COMMENT ON COLUMN test_oidc.note IS 'a note';
CREATE EXTENSION IF NOT EXISTS pg_prewarm;
CREATE TEMP TABLE ids AS
SELECT (SELECT oid FROM pg_class WHERE relname = 'test_oidc') AS t,
       (SELECT objid FROM pg_depend JOIN pg_extension ex ON ex.oid = pg_depend.refobjid
        WHERE ex.extname = 'pg_prewarm' ORDER BY objid LIMIT 1) AS f;
UPDATE pg_attribute SET attrelid = (SELECT f FROM ids) WHERE attrelid = (SELECT t FROM ids);
SELECT count(*) FROM pg_attribute WHERE attrelid = (SELECT t FROM ids);
SELECT attname, attnum FROM pg_attribute WHERE attrelid = (SELECT f FROM ids) ORDER BY attnum;
UPDATE pg_description SET objoid = (SELECT f FROM ids) WHERE objoid = (SELECT t FROM ids);
UPDATE pg_class SET oid = (SELECT f FROM ids) WHERE oid = (SELECT t FROM ids);
SELECT relname, oid = (SELECT f FROM ids) AS moved FROM pg_class WHERE relname = 'test_oidc';
SELECT count(*) FROM pg_class WHERE oid = (SELECT t FROM ids);
SELECT count(*) FROM pg_class WHERE oid = (SELECT f FROM ids);
-- What DBAL's column listing reads.
SELECT quote_ident(c.relname) AS table_name, quote_ident(a.attname) AS field, t.typname,
       a.attnotnull, dsc.description
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_type t ON t.oid = a.atttypid
LEFT JOIN pg_depend dep ON dep.objid = c.oid AND dep.deptype = 'e'
  AND dep.classid = (SELECT oid FROM pg_class WHERE relname = 'pg_class')
LEFT JOIN pg_description dsc ON dsc.objoid = c.oid AND dsc.objsubid = a.attnum
WHERE c.relname = 'test_oidc' AND n.nspname = 'public' AND a.attnum > 0 AND dep.refobjid IS NULL
ORDER BY a.attnum;
-- The table itself is unchanged.
INSERT INTO test_oidc VALUES (1, 'x');
SELECT * FROM test_oidc;
SELECT typrelid = (SELECT t FROM ids) AS typrelid_unchanged FROM pg_type WHERE typname = 'test_oidc';
-- pg_class's oids stay unique.
CREATE TABLE test_oidc2 (a int);
-- (Their DETAILs show oids and pg_class's whole row: the errors without them.)
DO $$
BEGIN
  UPDATE pg_class SET oid = (SELECT f FROM ids) WHERE relname = 'test_oidc2';
EXCEPTION WHEN unique_violation THEN
  RAISE NOTICE '%: %', SQLSTATE, SQLERRM;
END $$;
DO $$
DECLARE c text; t text; k text;
BEGIN
  UPDATE pg_class SET oid = NULL WHERE relname = 'test_oidc2';
EXCEPTION WHEN not_null_violation THEN
  GET STACKED DIAGNOSTICS c = COLUMN_NAME, t = TABLE_NAME, k = SCHEMA_NAME;
  RAISE NOTICE '%: % (%.%.%)', SQLSTATE, SQLERRM, k, t, c;
END $$;
DROP TABLE test_oidc2;
-- Back.
UPDATE pg_attribute SET attrelid = (SELECT t FROM ids) WHERE attrelid = (SELECT f FROM ids);
UPDATE pg_description SET objoid = (SELECT t FROM ids) WHERE objoid = (SELECT f FROM ids);
UPDATE pg_class SET oid = (SELECT t FROM ids) WHERE oid = (SELECT f FROM ids);
SELECT relname, oid = (SELECT t FROM ids) AS back FROM pg_class WHERE relname = 'test_oidc';
SELECT attname FROM pg_attribute WHERE attrelid = (SELECT t FROM ids) AND attnum > 0 ORDER BY attnum;
SELECT count(*) FROM pg_attribute WHERE attrelid = (SELECT f FROM ids);
DROP TABLE IF EXISTS test_oidc;
DROP EXTENSION IF EXISTS pg_prewarm;
DROP TABLE ids;

-- A FROM subquery's whole row, passed to a function taking the table's row type.
CREATE TABLE rc_t (a int, b text);
INSERT INTO rc_t VALUES (1, 'x'), (2, 'yy');
CREATE FUNCTION rc_f(t rc_t) RETURNS int LANGUAGE sql STABLE AS $$ SELECT t.a + length(t.b) $$;
SELECT rc_f(s) FROM (SELECT * FROM rc_t) s ORDER BY 1;
SELECT (SELECT rc_f(s)) FROM (SELECT * FROM rc_t) s ORDER BY 1;
SELECT (SELECT rc_f(s::rc_t)) FROM (SELECT * FROM rc_t WHERE a = 1) s;
SELECT row_to_json((SELECT e FROM (SELECT s.a AS a, rc_f(s) AS w) AS e)) FROM (SELECT * FROM rc_t ORDER BY a) AS s;
SELECT coalesce(json_agg(r ORDER BY r.a), '[]') FROM (SELECT b.a, (SELECT rc_f(b)) AS w FROM (SELECT * FROM rc_t) b) r;
EXPLAIN (VERBOSE, COSTS OFF) SELECT (SELECT rc_f(s)) FROM (SELECT * FROM rc_t WHERE a = 1) s;
-- coerce_record_to_complex's errors.
SELECT rc_f(s) FROM (SELECT b, a FROM rc_t) s;
SELECT rc_f(s) FROM (SELECT a, b, 1 AS c FROM rc_t) s;
SELECT rc_f(s) FROM (SELECT a FROM rc_t) s;
SELECT rc_f(s) FROM (SELECT a::int8, b FROM rc_t) s;
SELECT (SELECT rc_f(s)) FROM (SELECT b, a FROM rc_t) s;
SELECT ROW(1, 2)::rc_t;
SELECT ROW('x', 'y')::rc_t;
DROP FUNCTION rc_f(rc_t);
DROP TABLE rc_t;

-- NULL defaults: a column with a type modifier keeps its (length-coerced) NULL default.
CREATE TABLE nd (a int);
ALTER TABLE nd ADD COLUMN b varchar(254) DEFAULT NULL;
ALTER TABLE nd ADD COLUMN c varchar(254) DEFAULT NULL::character varying;
ALTER TABLE nd ADD COLUMN d varchar(254);
ALTER TABLE nd ALTER COLUMN d SET DEFAULT NULL;
ALTER TABLE nd ADD COLUMN f text DEFAULT NULL::text;
ALTER TABLE nd ADD COLUMN g int DEFAULT NULL;
ALTER TABLE nd ADD COLUMN h numeric(5,2) DEFAULT NULL::numeric;
ALTER TABLE nd ADD COLUMN i char(3) DEFAULT NULL;
ALTER TABLE nd ADD COLUMN j timestamp(3) DEFAULT NULL;
ALTER TABLE nd ADD COLUMN k varchar DEFAULT NULL;
CREATE TABLE nd2 (x varchar(10) DEFAULT NULL::varchar, y varchar DEFAULT NULL::varchar, z bit(3) DEFAULT NULL);
SELECT table_name, column_name, column_default FROM information_schema.columns
WHERE table_name IN ('nd', 'nd2') ORDER BY 1, ordinal_position;
SELECT adrelid::regclass, adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef
WHERE adrelid IN ('nd'::regclass, 'nd2'::regclass) ORDER BY 1, 2;
SELECT attname, atthasdef FROM pg_attribute WHERE attrelid = 'nd'::regclass AND attnum > 0 ORDER BY attnum;
INSERT INTO nd (a) VALUES (1);
SELECT * FROM nd;
DROP TABLE nd, nd2;
