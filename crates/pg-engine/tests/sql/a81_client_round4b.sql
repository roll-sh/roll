-- Client suites, round 4 (agent 81), second part.
-- Literal types without a length (ConstCharacter / ConstBit): EF Core's N'...', Postgrex.
SELECT N'VINET' AS n, pg_typeof(N'VINET') AS t, length(N'ab  ') AS len;
SELECT 'VINET'::pg_catalog.bpchar, bpchar 'VINET', char 'VINET', nchar 'abc', character 'xyz',
  'xy'::char, 'xy'::nchar, national character 'qq';
SELECT bit '101', '1'::bit, B'101'::bit, 'abc'::char, char(2) 'abc', varchar 'abc';
CREATE TABLE a81_o (a char, b bit, c character, d char(3), e bit varying, f nchar, g nchar(5));
INSERT INTO a81_o (a, g) VALUES ('z', N'VINET');
SELECT a, g FROM a81_o;
SELECT format_type(atttypid, atttypmod) FROM pg_attribute
  WHERE attrelid = 'a81_o'::regclass AND attnum > 0 ORDER BY attnum;
DROP TABLE a81_o;

-- Table constraints before the columns they name (MikroORM, sqlc).
CREATE TABLE a81_kp (key varchar(64) PRIMARY KEY);
CREATE TABLE a81_k (PRIMARY KEY (a, b) INCLUDE (c), UNIQUE (b), CHECK (b > 0),
  a varchar(64) NOT NULL REFERENCES a81_kp (key), b int NOT NULL UNIQUE, c int);
SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint
  WHERE conrelid = 'a81_k'::regclass ORDER BY conname;
DROP TABLE a81_k, a81_kp;

-- jOOQ, Liquibase, Flyway's catalog reads.
CREATE DOMAIN a81_d AS numeric(10,2) NOT NULL DEFAULT 0;
CREATE TABLE a81_r4 (id int, p a81_d, q int NOT NULL);
SELECT column_name, is_nullable FROM information_schema.columns
  WHERE table_name = 'a81_r4' ORDER BY ordinal_position;
SELECT information_schema._pg_truetypid(a.*, t.*), information_schema._pg_truetypmod(a.*, t.*)
  FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
  WHERE a.attrelid = 'a81_r4'::regclass AND a.attnum > 0 ORDER BY a.attnum;
CREATE INDEX a81_r4_i ON a81_r4 (q DESC, id NULLS FIRST, p);
SELECT i.indoption::int[], i.indkey::int[], i.indkey::int8[] FROM pg_index i
  WHERE indexrelid = 'a81_r4_i'::regclass;
CREATE SEQUENCE a81_seq START 5;
SELECT pg_sequence_last_value('a81_seq'::regclass);
SELECT nextval('a81_seq');
SELECT pg_sequence_last_value('a81_seq'::regclass);
SELECT pg_sequence_last_value('a81_r4'::regclass);
SELECT nextval('a81_r4');
SELECT * FROM pg_get_sequence_data('a81_r4');
CREATE FUNCTION a81_f(a int, b text) RETURNS int LANGUAGE sql AS 'select a';
SELECT oidvectortypes(proargtypes), oidvectortypes(''::oidvector),
  oidvectortypes('23 0 25'::oidvector) FROM pg_proc WHERE proname = 'a81_f';
DROP FUNCTION a81_f;
DROP SEQUENCE a81_seq;
DROP TABLE a81_r4;
DROP DOMAIN a81_d;

-- DISTINCT ON a sub-select (Ecto); row locks in a read-only transaction (Diesel).
CREATE TABLE a81_posts (id int, title text);
INSERT INTO a81_posts VALUES (1, 'a'), (2, 'b'), (3, 'a');
SELECT DISTINCT ON ((SELECT 1)) title FROM a81_posts ORDER BY (SELECT 1), title;
SELECT DISTINCT ON ((SELECT 1)) title FROM a81_posts ORDER BY (SELECT 2), title;
START TRANSACTION READ ONLY;
SELECT * FROM a81_posts FOR UPDATE;
ROLLBACK;
START TRANSACTION READ ONLY;
SELECT * FROM a81_posts FOR KEY SHARE;
ROLLBACK;
START TRANSACTION READ ONLY;
SELECT * FROM a81_nosuch FOR UPDATE;
ROLLBACK;

-- ON CONFLICT's arbiter is the planner's (sqlc's PREPARE).
CREATE TABLE a81_authors (id serial PRIMARY KEY, name text, bio text);
PREPARE a81_up(text, text) AS INSERT INTO a81_authors (name, bio) VALUES ($1, $2)
  ON CONFLICT (name) DO UPDATE SET bio = $2 RETURNING id, bio;
SELECT parameter_types, result_types FROM pg_prepared_statements WHERE name = 'a81_up';
EXECUTE a81_up('a', 'b');
DEALLOCATE a81_up;
DROP TABLE a81_posts, a81_authors;
