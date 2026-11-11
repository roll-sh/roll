-- Identifiers of NAMEDATALEN bytes or more are truncated by the scanner, with a NOTICE.
CREATE TABLE it_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx (a int);
CREATE TABLE "it_YYxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" (a int);
SELECT relname, length(relname) FROM pg_class WHERE relname LIKE 'it\_%' ORDER BY 1;
-- Multibyte characters are never split.
CREATE TABLE it_éééééééééééééééééééééééééééééééééééééééééééééééé (a int);
SELECT relname, octet_length(relname) FROM pg_class WHERE relname LIKE 'it\_é%' ORDER BY 1;
SELECT typname, octet_length(typname) FROM pg_type WHERE typname LIKE '%it\_é%' ORDER BY 1;
SELECT 1 AS xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx;
SELECT 1 AS "Lllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllllll""x";
SELECT 1 AS U&"\0041mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
-- Strings, comments and dollar quotes aren't identifiers.
SELECT 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' AS s, $q$yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy$q$ AS d; -- zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz
-- Two columns that truncate to the same name.
CREATE TABLE it_t2 (abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij_col1 int, abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij_col2 int);
-- Tokens after a syntax error aren't scanned.
SELECT 1 AS ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh;
-- Names read from strings are truncated silently.
SELECT 'iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii'::regclass;
SELECT 'a'::name, repeat('b', 70)::name;
-- client_min_messages hides the NOTICE.
SET client_min_messages = warning;
SELECT 1 AS kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk;
RESET client_min_messages;
-- Generated names (makeObjectName): the longer part is cut first, the label kept.
CREATE TABLE it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa int PRIMARY KEY, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb serial UNIQUE, CHECK (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa > 0));
CREATE INDEX ON it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa);
CREATE INDEX ON it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa);
CREATE INDEX ON it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb);
ALTER TABLE it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt ADD CHECK (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa > 1), ADD CHECK (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa > 2);
CREATE TABLE it_ref (x int REFERENCES it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt, yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy int REFERENCES it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt);
CREATE STATISTICS ON aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb FROM it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt;
CREATE STATISTICS ON aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb FROM it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt;
SELECT stxname FROM pg_statistic_ext WHERE stxrelid = 'it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt'::regclass ORDER BY 1;
SELECT conname FROM pg_constraint WHERE conrelid IN ('it_tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt'::regclass, 'it_ref'::regclass) ORDER BY 1;
SELECT relname FROM pg_class WHERE relname LIKE 'it\_ttt%' ORDER BY 1;
SELECT typname FROM pg_type WHERE typname LIKE '%it\_ttt%' ORDER BY 1;
-- A serial column's sequence name that is taken: the label is numbered, the name cut again.
CREATE TABLE it_dupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupd_seq (a int);
CREATE TABLE it_dupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdupdup (bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb serial);
SELECT relname FROM pg_class WHERE relname LIKE 'it\_dup%' ORDER BY 1;
-- Range-table names made unique stay within NAMEDATALEN.
CREATE TABLE it_q (q1 int, q2 int);
CREATE VIEW it_v AS
  SELECT * FROM it_q xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxy
  UNION ALL
  SELECT * FROM it_q xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxz;
SELECT pg_get_viewdef('it_v', true);
EXPLAIN (COSTS OFF) SELECT * FROM it_v;
-- SQL functions' bodies are scanned when created and when run.
CREATE FUNCTION it_f() RETURNS int LANGUAGE sql AS 'SELECT 1 AS aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
SELECT it_f();
-- PL/pgSQL variables are truncated like any identifier.
SET client_min_messages = warning;
CREATE FUNCTION it_g() RETURNS int LANGUAGE plpgsql AS $$
DECLARE bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb int := 7;
BEGIN RETURN bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb + 1; END $$;
SELECT it_g();
RESET client_min_messages;
DO $$ BEGIN EXECUTE 'SELECT 1 AS eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'; END $$;
