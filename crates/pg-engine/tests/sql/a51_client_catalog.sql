-- Catalog details clients read (round 18, agent 51).
-- makeArrayTypeName: a new type's array type whose `_name` another type already has gets
-- `_name_1`; a type made with an array type's name moves that one aside.
CREATE TYPE a51_x AS (a int);
CREATE TYPE "___a51_x" AS (a int);
CREATE TYPE "__a51_x" AS (a int);
SELECT typname, typelem::regtype FROM pg_type WHERE typname LIKE '%a51_x%' ORDER BY 1;
CREATE TYPE a51_custom AS (i int);
CREATE TYPE _a51_custom AS (f float8);
SELECT t.typname, e.typname AS elem, a.typname AS arr FROM pg_type t
  LEFT JOIN pg_type e ON e.oid = t.typelem LEFT JOIN pg_type a ON a.oid = t.typarray
  WHERE t.typname LIKE '%a51_custom%' ORDER BY 1;
CREATE TABLE a51_shadow (c1 a51_custom, c2 _a51_custom, c3 a51_custom[], c4 _a51_custom[]);
SELECT attname, format_type(atttypid, atttypmod), (SELECT typname FROM pg_type WHERE oid = atttypid)
  FROM pg_attribute WHERE attrelid = 'a51_shadow'::regclass AND attnum > 0 ORDER BY attnum;
DROP TABLE a51_shadow;
DROP TYPE a51_x, "___a51_x", "__a51_x", a51_custom, _a51_custom;

-- pg_table_is_visible: information_schema's views only with it on the search path;
-- pg_type_is_visible of relations' row types and their array types.
CREATE TABLE a51_testtable (id int PRIMARY KEY, data varchar(30));
CREATE VIEW a51_regview AS SELECT data FROM a51_testtable;
CREATE MATERIALIZED VIEW a51_mview AS SELECT * FROM a51_testtable;
SELECT c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE pg_table_is_visible(c.oid) AND n.nspname <> 'pg_catalog' AND c.relkind IN ('v', 'm')
  ORDER BY 1;
SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'information_schema' AND pg_table_is_visible(c.oid);
SET search_path = public, information_schema;
SELECT pg_table_is_visible('information_schema.tables'::regclass), pg_table_is_visible('pg_class'::regclass);
RESET search_path;
SELECT pg_table_is_visible('information_schema.tables'::regclass), pg_table_is_visible('pg_class'::regclass);
SELECT t.typname, pg_type_is_visible(t.oid), t.typtype FROM pg_type t
  WHERE t.typname IN ('a51_testtable', 'a51_regview', 'a51_mview', '_a51_mview', '_a51_testtable')
  ORDER BY 1;
CREATE SCHEMA a51_s;
CREATE TABLE a51_s.a51_testtable (x int);
SET search_path = a51_s, public;
SELECT t.typname, n.nspname, pg_type_is_visible(t.oid) FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
  WHERE t.typname IN ('a51_testtable', '_a51_testtable') ORDER BY 1, 2;
RESET search_path;
DROP SCHEMA a51_s CASCADE;
DROP MATERIALIZED VIEW a51_mview;
DROP VIEW a51_regview;
DROP TABLE a51_testtable;

-- MaxTupleAttributeNumber: target lists and ROW expressions.
DO $$BEGIN EXECUTE format('SELECT %s FROM pg_class LIMIT 1', (SELECT string_agg('relname', ',') FROM generate_series(1, 1665))); END$$;
DO $$BEGIN EXECUTE format('SELECT * FROM (VALUES (%s)) v', (SELECT string_agg('1', ',') FROM generate_series(1, 1665))); END$$;
DO $$BEGIN EXECUTE format('SELECT ROW(%s)', (SELECT string_agg('1', ',') FROM generate_series(1, 1665))); END$$;
DO $$BEGIN EXECUTE format('SELECT %s', (SELECT string_agg('1', ',') FROM generate_series(1, 1664))); END$$;

-- The largest numeric: 131072 digits before the point, 16383 after (its binary form has
-- more base-10000 digits than an int2 counts).
SELECT md5(x::text), scale(x) FROM (SELECT ('1' || repeat('0', 131070) || '.' || repeat('0', 16382) || '1')::numeric AS x) s;
