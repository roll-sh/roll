-- pg_class read right after DDL (a catalog version no statement has read yet): of the
-- relations its filter or a join fixes alone.
CREATE SCHEMA a166_cw;
CREATE TABLE a166_cw.t1 (id int PRIMARY KEY, a text);
CREATE TABLE a166_cw.t2 (id int);
CREATE INDEX a166_cw_t2_id ON a166_cw.t2 (id);
CREATE VIEW a166_cw.v AS SELECT 1 AS one;
CREATE SEQUENCE a166_cw.s;
CREATE TYPE a166_cw.comp AS (x int);
SELECT relname, relkind FROM pg_class WHERE relnamespace = 'a166_cw'::regnamespace ORDER BY relname;
CREATE TABLE a166_cw.t3 (id int);
SELECT c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'a166_cw' AND c.relkind IN ('r', 'v', 'c') ORDER BY 1;
CREATE TABLE a166_cw.t4 (id int);
SELECT count(*) FROM pg_class WHERE relnamespace = 'a166_cw'::regnamespace AND relkind = 'i';
CREATE TABLE a166_cw.t5 (id int);
SELECT relname FROM pg_class WHERE relkind = ANY ('{S}') AND relnamespace = 'a166_cw'::regnamespace;
CREATE TABLE a166_cw.t6 (id int);
SELECT relname FROM pg_class WHERE oid IN ('a166_cw.t1'::regclass, 'a166_cw.v'::regclass) ORDER BY 1;
CREATE TABLE a166_cw.t7 (id int);
SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname ~ '^(a166_cw)$' AND c.relkind IN ('r', 'p', '') ORDER BY 1, 2;
CREATE TABLE a166_cw.t8 (id int);
SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid
  WHERE c.relnamespace = 'a166_cw'::regnamespace AND a.attnum > 0;
CREATE TABLE a166_cw.t9 (id int);
SELECT c2.relname FROM pg_index i JOIN pg_class c2 ON c2.oid = i.indexrelid
  WHERE i.indrelid = 'a166_cw.t1'::regclass ORDER BY 1;
CREATE TABLE a166_cw.t10 (id int);
-- A value that fails to compute is the filter's: no error when no row reaches it.
SELECT count(*) FROM pg_class WHERE relkind = 'x' AND relnamespace = 'a166_no_such'::regnamespace;
SELECT count(*) FROM pg_class WHERE relnamespace = 'a166_no_such'::regnamespace;
CREATE TABLE a166_cw.t11 (id int);
SELECT count(*) FROM pg_class WHERE relnamespace IN ('a166_cw'::regnamespace, 'pg_toast'::regnamespace) AND relkind = 'v';
DROP SCHEMA a166_cw CASCADE;
