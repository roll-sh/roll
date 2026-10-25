-- What client libraries' test suites found (tests/clients): catalog queries ORMs run, casts
-- and coercions drivers rely on.
-- "char" to text is an implicit cast (SQLAlchemy's reflection: relkind = ANY (ARRAY['r', ...])).
SELECT count(*) > 0 FROM pg_class WHERE relkind = ANY (ARRAY['r', 'p', 'f', 'v', 'm']);
SELECT 'r'::"char" = 'r'::text, 'r'::"char" || 'x';
SELECT pg_typeof('r'::"char" || 'x');
-- text(inet) always shows the mask (asyncpg's text output of inet).
SELECT '10.11.12.13'::inet::text, '10.11.12.13/8'::inet::text, '::1'::inet::varchar, '10.0.0.0/8'::cidr::text;
SELECT text('10.11.12.13'::inet), host('10.11.12.13'::inet), '10.11.12.13'::inet;
-- ARRAY[] with a sub-array or array-typed element is multidimensional: the others are
-- coerced to the array type (psycopg2's nested lists of NULLs).
SELECT ARRAY['{NULL,NULL}', ARRAY[1, NULL]]::int[];
SELECT ARRAY['{1,2}', '{3,4}']::text[];
SELECT ARRAY[ARRAY[NULL, NULL], '{NULL,NULL}']::int[];
SELECT ARRAY['{NULL,NULL}', '{NULL,NULL}']::int[];
-- The transaction characteristics a transaction not started yet will have.
SET default_transaction_read_only = on;
SET default_transaction_deferrable = on;
SHOW transaction_read_only;
SHOW transaction_deferrable;
SELECT current_setting('transaction_read_only'), current_setting('transaction_deferrable');
BEGIN;
SHOW transaction_read_only;
SHOW transaction_deferrable;
ROLLBACK;
BEGIN READ WRITE NOT DEFERRABLE;
SHOW transaction_read_only;
SHOW transaction_deferrable;
ROLLBACK;
RESET default_transaction_read_only;
RESET default_transaction_deferrable;
-- A composite type's fields declared as a domain keep it in pg_attribute (pgx LoadTypes).
CREATE DOMAIN cc_u64 AS numeric(20,0);
CREATE TYPE cc_dt AS (a text, b int4, c cc_u64, d cc_u64[]);
SELECT attname, atttypid::regtype, attnum FROM pg_attribute
  WHERE attrelid = (SELECT typrelid FROM pg_type WHERE typname = 'cc_dt') AND attnum > 0 ORDER BY attnum;
DROP TYPE cc_dt;
DROP DOMAIN cc_u64;
-- Calls with a void argument drop it (asyncpg's codec test).
PREPARE cc_void(void) AS SELECT now($1) IS NOT NULL;
EXECUTE cc_void('');
DEALLOCATE cc_void;
-- DateStyle's key words that match by prefix (pgjdbc's DateStyle check).
SET DateStyle = 'PostgreSQL';
SHOW DateStyle;
SET DateStyle = 'European';
SHOW DateStyle;
SET DateStyle = 'NonEuropean';
SHOW DateStyle;
SET DateStyle = 'non-european';
RESET DateStyle;
-- An array column's type modifier is its element's (Npgsql's column schema).
CREATE TABLE cc_tm (a varchar(10)[], b numeric(10,2)[], c char(3)[], d bit(3)[]);
SELECT attname, atttypmod, attndims, format_type(atttypid, atttypmod) FROM pg_attribute
  WHERE attrelid = 'cc_tm'::regclass AND attnum > 0 ORDER BY attnum;
SELECT format_type('varchar[]'::regtype, 14), format_type('_numeric'::regtype, 655366);
-- Array elements get the column's length coercion.
INSERT INTO cc_tm (a) VALUES ('{abcdefghijk}');
SELECT '{abcdefgh}'::varchar(10)[]::varchar(5)[];
INSERT INTO cc_tm (a) VALUES ('{abcdefgh}');
ALTER TABLE cc_tm ALTER COLUMN a TYPE varchar(5)[];
ALTER TABLE cc_tm ALTER COLUMN a TYPE varchar(5)[] USING a::varchar(5)[];
SELECT a FROM cc_tm;
DROP TABLE cc_tm;
-- CREATE COLLATION collations in pg_attribute and pg_index (Django's introspection).
CREATE COLLATION cc_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE cc_c1 (a varchar(10)[] COLLATE cc_ci, b varchar(10) COLLATE cc_ci UNIQUE, c varchar(10) COLLATE cc_ci PRIMARY KEY);
SELECT c.relname, a.attname, co.collname FROM pg_attribute a LEFT JOIN pg_collation co ON a.attcollation = co.oid
  JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname IN ('cc_c1', 'cc_c1_b_key', 'cc_c1_pkey') AND a.attnum > 0 ORDER BY 1, 2;
SELECT indexrelid::regclass, (SELECT collname FROM pg_collation WHERE oid = indcollation[0]) FROM pg_index
  WHERE indrelid = 'cc_c1'::regclass ORDER BY 1;
DROP TABLE cc_c1;
DROP COLLATION cc_ci;
-- Errors running COPY's query.
CREATE FUNCTION cc_bad() RETURNS TABLE (i int) LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT s.a FROM pg_stat_activity p; END $$;
COPY (SELECT * FROM cc_bad()) TO STDOUT WITH (FORMAT csv);
DROP FUNCTION cc_bad();
