-- Client suites round 7 (agent 133).
-- Doctrine DBAL's renumbering of a table onto another object's oid: regclass input and
-- output follow pg_class's row, as PostgreSQL's syscache lookups do.
CREATE TABLE test_oidr (id int NOT NULL, note text);
CREATE EXTENSION IF NOT EXISTS pg_prewarm;
CREATE TEMP TABLE ids AS
SELECT (SELECT oid FROM pg_class WHERE relname = 'test_oidr') AS t,
       (SELECT objid FROM pg_depend JOIN pg_extension ex ON ex.oid = pg_depend.refobjid
        WHERE ex.extname = 'pg_prewarm' ORDER BY objid LIMIT 1) AS f;
UPDATE pg_attribute SET attrelid = (SELECT f FROM ids) WHERE attrelid = (SELECT t FROM ids);
UPDATE pg_class SET oid = (SELECT f FROM ids) WHERE oid = (SELECT t FROM ids);
SELECT 'test_oidr'::regclass = (SELECT f FROM ids) AS input_moved,
       'public.test_oidr'::regclass = (SELECT f FROM ids) AS qualified_moved;
SELECT to_regclass('test_oidr') = (SELECT f FROM ids) AS to_regclass_moved;
SELECT (SELECT f FROM ids)::regclass AS new_oid_out;
SELECT (SELECT t FROM ids)::regclass::text = (SELECT t FROM ids)::text AS old_oid_bare;
SELECT a.attname FROM pg_attribute a
WHERE a.attrelid = 'test_oidr'::regclass AND a.attnum > 0 ORDER BY a.attnum;
SELECT c.relname FROM pg_class c WHERE c.oid = 'test_oidr'::regclass;
-- The table itself works as before.
INSERT INTO test_oidr VALUES (1, 'x');
SELECT * FROM test_oidr;
UPDATE pg_attribute SET attrelid = (SELECT t FROM ids) WHERE attrelid = (SELECT f FROM ids);
UPDATE pg_class SET oid = (SELECT t FROM ids) WHERE oid = (SELECT f FROM ids);
SELECT 'test_oidr'::regclass = (SELECT t FROM ids) AS input_back;
SELECT (SELECT t FROM ids)::regclass AS own_oid_out;
DROP TABLE test_oidr;
DROP EXTENSION pg_prewarm;

-- ALTER COLUMN ... TYPE keeps the cooked default, coerced implicitly: it shows as it did
-- under the old type (Supabase Auth's auth.users.phone_change: varchar(15) DEFAULT '' made
-- text), and converts from that type.
CREATE TABLE a133_ct (a varchar(15) DEFAULT '', b varchar(15) DEFAULT 'x', c int DEFAULT 1,
  d varchar(10) DEFAULT NULL, e varchar(5) DEFAULT 'ab'::text, f text DEFAULT 'q',
  g numeric(5,2) DEFAULT 1.5, h char(3) DEFAULT 'z', i varchar(4) DEFAULT lower('A'));
ALTER TABLE a133_ct ALTER COLUMN a TYPE text, ALTER COLUMN b TYPE varchar(30),
  ALTER COLUMN c TYPE bigint, ALTER COLUMN d TYPE text, ALTER COLUMN e TYPE text,
  ALTER COLUMN f TYPE varchar(9), ALTER COLUMN g TYPE numeric(8,3), ALTER COLUMN h TYPE text,
  ALTER COLUMN i TYPE text;
SELECT attname, pg_get_expr(adbin, adrelid) FROM pg_attrdef
  JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum
WHERE adrelid = 'a133_ct'::regclass ORDER BY adnum;
SELECT column_name, column_default FROM information_schema.columns
WHERE table_name = 'a133_ct' ORDER BY ordinal_position;
DROP TABLE a133_ct;
CREATE TABLE a133_ct (a varchar(15) DEFAULT '', b varchar(15) DEFAULT '7');
ALTER TABLE a133_ct ALTER COLUMN a TYPE int;
ALTER TABLE a133_ct ALTER COLUMN b TYPE int;
ALTER TABLE a133_ct ALTER COLUMN b TYPE int USING b::int;
SELECT attname, pg_get_expr(adbin, adrelid) FROM pg_attrdef
  JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum
WHERE adrelid = 'a133_ct'::regclass ORDER BY adnum;
DROP TABLE a133_ct;
CREATE TABLE a133_ct (a int DEFAULT 1 + 2, b timestamp DEFAULT now(), c text DEFAULT 'x' || 'y',
  d varchar(3)[] DEFAULT '{a}', e int[] DEFAULT array[1]);
ALTER TABLE a133_ct ALTER COLUMN a TYPE bigint, ALTER COLUMN b TYPE timestamptz,
  ALTER COLUMN c TYPE varchar(5), ALTER COLUMN d TYPE text[], ALTER COLUMN e TYPE bigint[];
SELECT attname, pg_get_expr(adbin, adrelid) FROM pg_attrdef
  JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum
WHERE adrelid = 'a133_ct'::regclass ORDER BY adnum;
INSERT INTO a133_ct DEFAULT VALUES;
SELECT a, b IS NOT NULL AS b, c, d, e FROM a133_ct;
DROP TABLE a133_ct;
