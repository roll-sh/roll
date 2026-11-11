-- Catalog work kept to what a statement touches (round 18, agent 58): DROP's dependents found
-- by deriving only what may depend on the objects reached (views, functions, row types,
-- partitions, constraints, triggers, policies, rules, statistics), pg_class read through
-- its name index (psql's \d patterns), pg_index rows of one relation, information_schema
-- rows of one relation, right after DDL, inside transactions and savepoints.
CREATE SCHEMA a58;
SET search_path = a58, public;
CREATE TABLE a58.p (id int PRIMARY KEY, note text);
CREATE TABLE a58.c (id serial PRIMARY KEY, p_id int REFERENCES a58.p, body text DEFAULT 'x');
CREATE VIEW a58.v1 AS SELECT id, note FROM a58.p;
CREATE VIEW a58.v2 AS SELECT id FROM a58.v1 WHERE id > 0;
CREATE VIEW a58."MixedCase" AS SELECT body FROM a58.c;
-- Views on views, foreign keys: refused, then cascaded.
DROP TABLE a58.p;
DROP VIEW a58.v1;
BEGIN;
DROP TABLE a58.p CASCADE;
SELECT relname FROM pg_class WHERE relnamespace = 'a58'::regnamespace ORDER BY 1;
ROLLBACK;
SELECT relname FROM pg_class WHERE relnamespace = 'a58'::regnamespace ORDER BY 1;
-- A function returning the table's row type, a view over it, a SQL-standard body.
CREATE FUNCTION a58.getp(int) RETURNS a58.p LANGUAGE sql AS 'SELECT * FROM a58.p WHERE id = $1';
CREATE VIEW a58.v3 AS SELECT note FROM a58.getp(1);
CREATE FUNCTION a58.countc() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM a58.c; END;
CREATE FUNCTION a58.plain() RETURNS int LANGUAGE sql AS 'SELECT 1';
DROP TABLE a58.c;
DROP TABLE a58.p;
BEGIN;
SAVEPOINT s;
DROP TABLE a58.c CASCADE;
ROLLBACK TO s;
DROP FUNCTION a58.countc();
DROP TABLE a58.c CASCADE;
SELECT proname FROM pg_proc WHERE pronamespace = 'a58'::regnamespace ORDER BY 1;
COMMIT;
-- A domain over the row type; a column of the row type; a typed default.
CREATE DOMAIN a58.pd AS a58.p CHECK ((VALUE).id > 0);
CREATE TABLE a58.holder (k int, r a58.p, d a58.pd);
DROP TABLE a58.p;
-- Partitions, their indexes and keys; triggers, policies, rules, statistics.
CREATE TABLE a58.pt (k int NOT NULL, v text, PRIMARY KEY (k)) PARTITION BY RANGE (k);
CREATE TABLE a58.pt1 PARTITION OF a58.pt FOR VALUES FROM (0) TO (10);
CREATE TABLE a58.pt2 PARTITION OF a58.pt FOR VALUES FROM (10) TO (20);
CREATE INDEX ON a58.pt (v);
CREATE TABLE a58.ref (k int REFERENCES a58.pt);
CREATE FUNCTION a58.trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER t1 BEFORE INSERT ON a58.pt FOR EACH ROW EXECUTE FUNCTION a58.trg();
CREATE POLICY pol ON a58.pt1 USING (k > 0);
CREATE STATISTICS a58.st ON k, v FROM a58.pt1;
CREATE TABLE a58.log (k int);
CREATE RULE r1 AS ON INSERT TO a58.log DO ALSO SELECT count(*) FROM a58.pt2;
DROP TABLE a58.pt1;
DROP TABLE a58.pt;
BEGIN;
DROP TABLE a58.pt CASCADE;
SELECT relname FROM pg_class WHERE relnamespace = 'a58'::regnamespace ORDER BY 1;
ROLLBACK;
DROP INDEX a58.pt_v_idx;
DROP INDEX a58.pt1_v_idx;
ALTER TABLE a58.pt DROP CONSTRAINT pt_pkey;
ALTER TABLE a58.pt DROP CONSTRAINT pt_pkey CASCADE;
DROP TABLE a58.pt2 CASCADE;
DROP TABLE a58.pt CASCADE;
SELECT relname FROM pg_class WHERE relnamespace = 'a58'::regnamespace ORDER BY 1;
-- A view grouping by a key relies on it.
CREATE TABLE a58.art (id int PRIMARY KEY, title text);
CREATE VIEW a58.artv AS SELECT id, title FROM a58.art GROUP BY id;
ALTER TABLE a58.art DROP CONSTRAINT art_pkey;
DROP VIEW a58.artv;
ALTER TABLE a58.art DROP CONSTRAINT art_pkey;
-- A view's rule, a sequence a default names, an index an expression names.
DROP RULE "_RETURN" ON a58.v2;
CREATE SEQUENCE a58.s1;
CREATE TABLE a58.useseq (k int DEFAULT nextval('a58.s1'));
DROP SEQUENCE a58.s1;
DROP SEQUENCE a58.s1 CASCADE;
SELECT a.attname, (SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum) FROM pg_attribute a WHERE a.attrelid = 'a58.useseq'::regclass AND a.attnum > 0 ORDER BY a.attnum;
-- psql's \d patterns: pg_class through its name index, right after DDL.
CREATE TABLE a58.tab_one (a int);
CREATE TABLE a58.tab_two (a int);
CREATE TABLE a58."Tab_Up" (a int);
BEGIN;
CREATE TABLE a58.tab_three (a int);
SELECT c.relname FROM pg_class c WHERE c.relname ~ '^(tab_three)$' ORDER BY 1;
SAVEPOINT s;
ALTER TABLE a58.tab_three RENAME TO tab_four;
SELECT c.relname FROM pg_class c WHERE c.relname ~ '^(tab_.*)$' ORDER BY 1;
ROLLBACK TO s;
SELECT c.relname FROM pg_class c WHERE c.relname ~ '^(tab_.*)$' ORDER BY 1;
COMMIT;
SELECT c.relname FROM pg_class c WHERE c.relname = 'Tab_Up';
SELECT c.relname FROM pg_class c WHERE c.relname ~ '^(Tab.*)$' ORDER BY 1;
SELECT c.relname FROM pg_class c WHERE c.relname >= 'tab_o' AND c.relname < 'tab_u' ORDER BY 1;
SELECT c.relname FROM pg_class c WHERE c.relname ~ '^(pg_class)$';
SELECT count(*) > 0 FROM pg_class c WHERE c.relname ~ '^(pg_toast_.*)$';
SELECT c.relname FROM pg_class c WHERE c.relname ~ '^(nosuch)$';
SELECT n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p') AND c.relname OPERATOR(pg_catalog.~) '^(tab.*)$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(a58)$' COLLATE pg_catalog.default ORDER BY 1, 2;
SELECT c.relname, n.nspname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relname OPERATOR(pg_catalog.~) '^(tab_one)$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(a58)$' COLLATE pg_catalog.default ORDER BY 2, 1;
-- pg_index of one table right after DDL.
CREATE INDEX tab_one_a ON a58.tab_one (a);
SELECT indexrelid::regclass, indnatts, indisunique FROM pg_index WHERE indrelid = 'a58.tab_one'::regclass;
SELECT count(*) FROM pg_index WHERE indrelid = 'a58.tab_two'::regclass;
-- information_schema of one relation.
SELECT table_name, column_name, ordinal_position, data_type, column_default
  FROM information_schema.columns WHERE table_schema = 'a58' AND table_name = 'useseq'
  ORDER BY ordinal_position;
SELECT column_name FROM information_schema.columns WHERE table_name = 'Tab_Up';
SELECT column_name FROM information_schema.columns WHERE table_name = 'tab_up';
SELECT column_name FROM information_schema.columns
  WHERE table_name = 'tab_one' AND table_name = 'tab_two';
SELECT table_schema, column_name FROM information_schema.columns
  WHERE table_name = 'pg_am' ORDER BY 1, 2;
SELECT table_name, table_type FROM information_schema.tables
  WHERE table_schema = 'a58' ORDER BY 1;
SELECT table_name, table_type FROM information_schema.tables
  WHERE table_schema = 'a58' AND table_name = 'v2';
BEGIN;
ALTER TABLE a58.tab_two ADD COLUMN b text DEFAULT 'q';
SELECT column_name, column_default FROM information_schema.columns
  WHERE table_schema = 'a58' AND table_name = 'tab_two' ORDER BY ordinal_position;
ROLLBACK;
SELECT column_name, column_default FROM information_schema.columns
  WHERE table_schema = 'a58' AND table_name = 'tab_two' ORDER BY ordinal_position;
DROP SCHEMA a58 CASCADE;
