-- Catalog lookups by name and oid across DDL, rollbacks and savepoints: schemas by name,
-- TOAST tables' owners, composite and row types (and their array types), relations' ACLs,
-- tables with row-level security, as statements see each catalog version.
CREATE SCHEMA cl_a;
CREATE TABLE cl_a.tt (id int PRIMARY KEY, body text);
CREATE TYPE cl_a.pair AS (x int, y text);
SET search_path = cl_a, public;
SELECT c.relname = 'pg_toast_' || t.oid AS toast_named, n.nspname
  FROM pg_class c JOIN pg_class t ON c.oid = t.reltoastrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace WHERE t.relname = 'tt';
SELECT format_type(t.oid, NULL), format_type(t.typarray, NULL), t.typtype, t.typinput
  FROM pg_type t WHERE t.typname IN ('tt', 'pair') ORDER BY 1;
SELECT a.attname, format_type(a.atttypid, a.atttypmod)
  FROM pg_attribute a JOIN pg_type t ON t.typrelid = a.attrelid
  WHERE t.typname = 'pair' AND a.attnum > 0 ORDER BY a.attnum;
BEGIN;
ALTER SCHEMA cl_a RENAME TO cl_b;
SELECT nspname FROM pg_namespace WHERE nspname LIKE 'cl_%' ORDER BY 1;
SELECT 'cl_b.tt'::regclass, 'cl_b.pair'::regtype, 'cl_b.pair[]'::regtype;
SAVEPOINT s1;
CREATE SCHEMA cl_a;
CREATE TABLE cl_a.tt (z int);
SELECT n.nspname, c.relname, format_type(c.reltype, NULL)
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relname = 'tt' ORDER BY 1;
ROLLBACK TO SAVEPOINT s1;
SELECT nspname FROM pg_namespace WHERE nspname LIKE 'cl_%' ORDER BY 1;
ALTER TYPE cl_b.pair RENAME TO couple;
SELECT format_type(t.oid, NULL), format_type(t.typarray, NULL)
  FROM pg_type t WHERE t.typname IN ('pair', 'couple', '_couple', '_pair') ORDER BY 1;
ROLLBACK;
SELECT nspname FROM pg_namespace WHERE nspname LIKE 'cl_%' ORDER BY 1;
SELECT format_type(t.oid, NULL), format_type(t.typarray, NULL)
  FROM pg_type t WHERE t.typname IN ('pair', 'couple') ORDER BY 1;
-- ACLs and row-level security as they change.
DROP ROLE IF EXISTS regress_cl_r;
CREATE ROLE regress_cl_r;
GRANT USAGE ON SCHEMA cl_a TO regress_cl_r;
GRANT SELECT ON cl_a.tt TO regress_cl_r;
INSERT INTO cl_a.tt VALUES (1, 'one'), (2, 'two');
SELECT relacl FROM pg_class WHERE relname = 'tt';
SELECT has_table_privilege('regress_cl_r', 'cl_a.tt', 'SELECT');
ALTER TABLE cl_a.tt ENABLE ROW LEVEL SECURITY;
CREATE POLICY p_one ON cl_a.tt TO regress_cl_r USING (id = 1);
SET ROLE regress_cl_r;
SELECT * FROM cl_a.tt ORDER BY id;
RESET ROLE;
BEGIN;
DROP POLICY p_one ON cl_a.tt;
ALTER TABLE cl_a.tt DISABLE ROW LEVEL SECURITY;
REVOKE SELECT ON cl_a.tt FROM regress_cl_r;
SELECT relacl, relrowsecurity FROM pg_class WHERE relname = 'tt';
ROLLBACK;
SELECT relacl, relrowsecurity FROM pg_class WHERE relname = 'tt';
SET ROLE regress_cl_r;
SELECT * FROM cl_a.tt ORDER BY id;
RESET ROLE;
-- A table renamed over a dropped one's name.
BEGIN;
ALTER TABLE cl_a.tt RENAME TO tt_old;
CREATE TABLE cl_a.tt (w text);
SELECT c.relname, t.relname IS NOT NULL AS has_toast
  FROM pg_class t JOIN pg_class c ON c.reltoastrelid = t.oid
  WHERE c.relnamespace = 'cl_a'::regnamespace ORDER BY 1;
COMMIT;
SELECT c.relname, format_type(c.reltype, NULL)
  FROM pg_class c WHERE c.relnamespace = 'cl_a'::regnamespace AND c.relkind = 'r' ORDER BY 1;
RESET search_path;
DROP SCHEMA cl_a CASCADE;
DROP ROLE regress_cl_r;
