-- Client suites, round 5 (agent 100), PostgREST: computed columns as schema.table.function,
-- the missing-column error naming the relation alone, ExecCheckXactReadOnly in functions'
-- statements, column definition lists' domains checked as the function returns its rows,
-- and FROM-clause functions run to their end before their first row goes up.
CREATE SCHEMA a100_api;
SET search_path = a100_api, public;
CREATE TABLE a100_api.authors (id int PRIMARY KEY, name text);
CREATE TABLE a100_api.books (id serial PRIMARY KEY, author_id int REFERENCES a100_api.authors, title text, pages int);
INSERT INTO a100_api.authors VALUES (1, 'Le Guin'), (2, 'Lem');
INSERT INTO a100_api.books (author_id, title, pages) VALUES (1, 'Earthsea', 183), (1, 'Dispossessed', 387), (2, 'Solaris', 204);
CREATE FUNCTION a100_api.book_count(a100_api.authors) RETURNS bigint LANGUAGE sql STABLE
  AS 'SELECT count(*) FROM a100_api.books WHERE author_id = $1.id';
CREATE FUNCTION a100_api.full_title(a100_api.books) RETURNS text LANGUAGE sql STABLE
  AS $$ SELECT $1.title || ' (' || $1.pages || ')' $$;
SELECT "a100_api"."authors"."name", "a100_api"."authors"."book_count" FROM "a100_api"."authors" ORDER BY 1;
SELECT a100_api.books.full_title FROM a100_api.books WHERE a100_api.books.book_count IS NULL;
SELECT a100_api.books.full_title FROM a100_api.books ORDER BY 1;
SELECT name FROM a100_api.authors WHERE a100_api.authors.book_count > 1;
SELECT "a100_api"."authors"."nosuch" FROM "a100_api"."authors";
SELECT authors.nosuch FROM a100_api.authors;
SELECT a100_api.authors.id FROM a100_api.authors x;
RESET search_path;
SELECT a100_api.authors.book_count FROM a100_api.authors;
DROP SCHEMA a100_api CASCADE;

-- A read-only transaction refuses a function's INSERT / UPDATE / DELETE before it runs,
-- named by the statement's command, unless it only reads or writes temporary tables.
CREATE TABLE a100_ro (a int);
CREATE TEMP TABLE a100_ro_tmp (a int);
CREATE FUNCTION a100_ro_upd() RETURNS int LANGUAGE plpgsql AS $$ BEGIN UPDATE a100_ro SET a = 1 WHERE false; RETURN 1; END $$;
CREATE FUNCTION a100_ro_tmp_f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO a100_ro_tmp VALUES (1); DELETE FROM a100_ro_tmp; RETURN 2; END $$;
CREATE FUNCTION a100_ro_sql() RETURNS int LANGUAGE sql AS $$ DELETE FROM a100_ro WHERE false; SELECT 3 $$;
CREATE FUNCTION a100_ro_merge() RETURNS int LANGUAGE plpgsql AS $$ BEGIN MERGE INTO a100_ro USING (SELECT 1 a) s ON false WHEN NOT MATCHED THEN DO NOTHING; RETURN 4; END $$;
CREATE FUNCTION a100_ro_dyn() RETURNS int LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'INSERT INTO a100_ro SELECT 1 WHERE false'; RETURN 5; END $$;
CREATE FUNCTION a100_ro_lock() RETURNS int LANGUAGE plpgsql AS $$ BEGIN PERFORM * FROM a100_ro FOR UPDATE; RETURN 6; END $$;
BEGIN READ ONLY;
SELECT a100_ro_upd();
ROLLBACK;
BEGIN READ ONLY;
SELECT a100_ro_tmp_f();
ROLLBACK;
BEGIN READ ONLY;
SELECT a100_ro_sql();
ROLLBACK;
BEGIN READ ONLY;
SELECT a100_ro_merge();
ROLLBACK;
BEGIN READ ONLY;
SELECT a100_ro_dyn();
ROLLBACK;
BEGIN READ ONLY;
SELECT a100_ro_lock();
ROLLBACK;
BEGIN READ ONLY;
WITH x AS (INSERT INTO a100_ro VALUES (1) RETURNING a) SELECT * FROM x;
ROLLBACK;
BEGIN READ ONLY;
WITH x AS (DELETE FROM a100_ro RETURNING a) SELECT * FROM x;
ROLLBACK;
BEGIN READ ONLY;
DO $$ BEGIN UPDATE a100_ro SET a = 2; END $$;
ROLLBACK;
DROP FUNCTION a100_ro_upd(), a100_ro_tmp_f(), a100_ro_sql(), a100_ro_merge(), a100_ro_dyn(), a100_ro_lock();
DROP TABLE a100_ro;

-- json_to_record's column definition list with a domain: checked as the row is made (read
-- or not), before the INSERT's default takes a sequence value; a set-returning function in
-- FROM makes all its rows before the first goes up.
CREATE DOMAIN a100_digits AS text CHECK (VALUE ~ '^[0-9]+$');
CREATE TABLE a100_sq (id serial, a a100_digits, b text);
INSERT INTO a100_sq (a, b) SELECT 'x', 'y';
SELECT last_value FROM a100_sq_id_seq;
INSERT INTO a100_sq (a, b) SELECT r.a, r.b FROM json_to_record('{"a": "x", "b": "y"}') AS r(a a100_digits, b text);
INSERT INTO a100_sq (a, b) SELECT r.a, r.b FROM (SELECT '{"a": "x"}'::json AS j) p,
  LATERAL (SELECT a, b FROM json_to_record(p.j) AS _(a a100_digits, b text)) r;
INSERT INTO a100_sq (a, b) SELECT r.a, r.b FROM jsonb_to_recordset('[{"a": "1"}, {"a": "x"}]') AS r(a a100_digits, b text);
INSERT INTO a100_sq (a, b) SELECT r.a, r.b FROM json_populate_record(null::a100_sq, '{"a": "x"}') AS r;
SELECT last_value FROM a100_sq_id_seq;
SELECT count(*) FROM json_to_record('{"a": "x"}') AS r(a a100_digits);
SELECT * FROM json_to_record('{"a": "x"}') AS r(a a100_digits) WHERE false;
SELECT count(*) FROM jsonb_to_recordset('[{"a": "12"}, {"a": "34"}]') AS r(a a100_digits);
SELECT x FROM jsonb_array_elements_text('["1", "2", "x"]') x WHERE x::int > 0;
SELECT count(*) FROM a100_sq;
DROP TABLE a100_sq;
DROP DOMAIN a100_digits;

-- An index scan's equal keys come in their HOT chains' roots' order (what the btree entries
-- point to): a row updated in place keeps its place among them (PostgREST's PATCH ...
-- RETURNING after an earlier update).
CREATE TABLE a100_hot (id int PRIMARY KEY, k int, v numeric);
CREATE INDEX a100_hot_k ON a100_hot (k);
INSERT INTO a100_hot SELECT g, g % 2, g FROM generate_series(1, 8) g;
UPDATE a100_hot SET v = v + 1 WHERE id = 2;
UPDATE a100_hot SET v = v + 1 WHERE id = 4;
SELECT id, ctid FROM a100_hot ORDER BY id;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT id FROM a100_hot WHERE k = 0;
BEGIN;
UPDATE a100_hot SET v = 0 WHERE k = 0 RETURNING id;
ROLLBACK;
SET enable_indexscan = off;
SET enable_bitmapscan = on;
SELECT id FROM a100_hot WHERE k = 0;
RESET enable_seqscan;
RESET enable_bitmapscan;
RESET enable_indexscan;
SELECT id FROM a100_hot;
DROP TABLE a100_hot;
