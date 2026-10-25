-- DDL over the relation maps' indexes (round 18, agent 49): foreign keys' internal triggers
-- and relhastriggers across drops, constraint names chosen in a schema, row types used by
-- columns, owned sequences, TOAST owners, defaults' and constraints' owners in cascaded
-- drops, DROP TABLE's dependents (scoped and not), and catalog scans pinned to one relation
-- right after DDL, inside transactions and savepoints.
CREATE SCHEMA ci;
SET search_path = ci, public;
CREATE TABLE ci.p (id int PRIMARY KEY, note text);
CREATE TABLE ci.c (id serial PRIMARY KEY, p_id int REFERENCES ci.p, body text DEFAULT 'x');
SELECT relname, relhastriggers FROM pg_class WHERE relnamespace = 'ci'::regnamespace
  AND relkind = 'r' ORDER BY 1;
-- The key dropped: both tables keep relhastriggers until VACUUM.
ALTER TABLE ci.c DROP CONSTRAINT c_p_id_fkey;
SELECT relname, relhastriggers FROM pg_class WHERE relnamespace = 'ci'::regnamespace
  AND relkind = 'r' ORDER BY 1;
VACUUM ci.p;
SELECT relname, relhastriggers FROM pg_class WHERE relnamespace = 'ci'::regnamespace
  AND relkind = 'r' ORDER BY 1;
-- Constraint names are unique in the schema: a second table's key picks another.
CREATE TABLE ci.d (x int, CONSTRAINT p_pkey CHECK (x > 0));
CREATE TABLE ci.e (x int);
ALTER TABLE ci.e ADD CONSTRAINT p_pkey UNIQUE (x);
ALTER TABLE ci.e ADD UNIQUE (x);
SELECT conrelid::regclass, conname, contype FROM pg_constraint
  WHERE connamespace = 'ci'::regnamespace ORDER BY 1, 2;
-- A column of a table's row type.
CREATE TABLE ci.holder (k int, r ci.p);
ALTER TABLE ci.p ADD COLUMN extra int DEFAULT 1;
ALTER TABLE ci.p ADD COLUMN extra2 int;
DROP TABLE ci.holder;
ALTER TABLE ci.p ADD COLUMN extra int DEFAULT 1;
-- One relation's rows, right after DDL.
COMMENT ON TABLE ci.c IS 'c';
SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, a.atthasdef,
  (SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d
     WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef)
  FROM pg_attribute a WHERE a.attrelid = 'ci.c'::regclass AND a.attnum > 0
  AND NOT a.attisdropped ORDER BY a.attnum;
SELECT relname, relkind, relnatts, relhasindex, relchecks FROM pg_class
  WHERE oid = 'ci.c'::regclass;
SELECT count(*) FROM pg_attribute WHERE attrelid = 'ci.c'::regclass;
SELECT count(*) FROM pg_attribute WHERE attrelid = NULL;
SELECT relname FROM pg_class WHERE oid = 0;
SELECT c.relname = 'pg_toast_' || t.oid, c.relkind FROM pg_class c, pg_class t
  WHERE t.oid = 'ci.c'::regclass AND c.oid = t.reltoastrelid;
BEGIN;
ALTER TABLE ci.c ADD COLUMN later date DEFAULT '2020-01-01';
SELECT attname FROM pg_attribute WHERE attrelid = 'ci.c'::regclass AND attnum > 0
  ORDER BY attnum;
SAVEPOINT s;
ALTER TABLE ci.c DROP COLUMN body;
SELECT attname, attisdropped FROM pg_attribute WHERE attrelid = 'ci.c'::regclass
  AND attnum > 0 ORDER BY attnum;
SELECT adnum FROM pg_attrdef WHERE adrelid = 'ci.c'::regclass ORDER BY 1;
ROLLBACK TO SAVEPOINT s;
SELECT adnum FROM pg_attrdef WHERE adrelid = 'ci.c'::regclass ORDER BY 1;
ROLLBACK;
SELECT attname FROM pg_attribute WHERE attrelid = 'ci.c'::regclass AND attnum > 0
  ORDER BY attnum;
-- DROP TABLE: owned sequences and the TOAST table go; a referencing key needs CASCADE.
CREATE TABLE ci.q (id int PRIMARY KEY);
CREATE TABLE ci.r (id serial, q_id int REFERENCES ci.q, t text);
DROP TABLE ci.q;
DROP TABLE ci.q CASCADE;
SELECT conname FROM pg_constraint WHERE conrelid = 'ci.r'::regclass ORDER BY 1;
SELECT relname, relhastriggers FROM pg_class WHERE oid = 'ci.r'::regclass;
DROP TABLE ci.r;
SELECT relname FROM pg_class WHERE relnamespace = 'ci'::regnamespace ORDER BY 1;
-- Dependents that aren't the table's own: a view, a default naming its sequence, a
-- function's body, an inheriting table.
CREATE TABLE ci.s (id serial PRIMARY KEY, v int);
CREATE TABLE ci.u (n int DEFAULT nextval('ci.s_id_seq'));
CREATE VIEW ci.sv AS SELECT id FROM ci.s;
DROP TABLE ci.s;
DROP VIEW ci.sv;
DROP TABLE ci.s;
DROP TABLE ci.s CASCADE;
SELECT adrelid::regclass, pg_get_expr(adbin, adrelid) FROM pg_attrdef
  WHERE adrelid = 'ci.u'::regclass;
CREATE TABLE ci.base (a int);
CREATE TABLE ci.kid (b int) INHERITS (ci.base);
DROP TABLE ci.base;
DROP TABLE ci.base CASCADE;
CREATE TABLE ci.w (id int);
CREATE FUNCTION ci.wf() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM ci.w; END;
DROP TABLE ci.w;
DROP FUNCTION ci.wf();
DROP TABLE ci.w;
-- Many tables: names, keys and drops in one transaction.
BEGIN;
CREATE TABLE ci.m1 (id int PRIMARY KEY);
CREATE TABLE ci.m2 (id int PRIMARY KEY, m1 int REFERENCES ci.m1);
CREATE TABLE ci.m3 (id int PRIMARY KEY, m2 int REFERENCES ci.m2);
SELECT relname, relhastriggers FROM pg_class WHERE relname LIKE 'm_' AND
  relnamespace = 'ci'::regnamespace ORDER BY 1;
DROP TABLE ci.m2 CASCADE;
SELECT relname, relhastriggers FROM pg_class WHERE relname LIKE 'm_' AND
  relnamespace = 'ci'::regnamespace ORDER BY 1;
SELECT conrelid::regclass, conname FROM pg_constraint WHERE connamespace = 'ci'::regnamespace
  AND conrelid::regclass::text LIKE 'm%' ORDER BY 1, 2;
COMMIT;
DROP SCHEMA ci CASCADE;
