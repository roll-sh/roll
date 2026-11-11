-- Composite values in an ICU-default database: fields without a COLLATE compare under the
-- database default (ICU), fields with one under theirs, as record_cmp does; so do
-- anonymous rows, arrays, nested composites, domains and ranges over them, sorts, min /
-- max, grouping, DISTINCT, unique indexes under a nondeterministic field collation, and
-- btree indexes on composite columns.
CREATE DATABASE locale_comp LOCALE_PROVIDER icu ICU_LOCALE 'en' LOCALE 'C' TEMPLATE template0;
\c locale_comp
CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TYPE ct AS (a text, b int);
CREATE TYPE cc AS (a text COLLATE "C", b int);
CREATE TYPE cn AS (a text COLLATE ci, b int);
CREATE TYPE cnest AS (x ct, y int);
CREATE TYPE ctr AS RANGE (subtype = ct);
CREATE DOMAIN dct AS ct;
SELECT ROW('a',1)::ct < ROW('B',1)::ct AS ct_icu, ROW('a',1)::cc < ROW('B',1)::cc AS cc_c;
SELECT r1 < r2 AS anon FROM (SELECT ROW('a'::text,1) r1, ROW('B'::text,1) r2) s;
SELECT r1 < r2 AS anon_c FROM (SELECT ROW('a'::text COLLATE "C",1) r1, ROW('B'::text COLLATE "C",1) r2) s;
SELECT ROW('a',1)::cn = ROW('A',1)::cn AS cn_eq;
SELECT ARRAY[ROW('a',1)::ct] < ARRAY[ROW('B',1)::ct] AS arr;
SELECT ROW(ROW('a',1)::ct, 1)::cnest < ROW(ROW('B',1)::ct, 1)::cnest AS nest;
SELECT ctr(ROW('a',1)::ct, ROW('B',1)::ct) AS rng;
SELECT ctr(ROW('a',1)::ct, ROW('c',1)::ct) @> ROW('B',1)::ct AS rng_contains;
SELECT ROW('a',1)::dct < ROW('B',1)::dct AS dom;
CREATE TABLE tn (c cn);
INSERT INTO tn VALUES (ROW('a',1)),(ROW('A',1)),(ROW('b',1)),(ROW('B',2));
SELECT count(DISTINCT c) FROM tn;
SELECT c, count(*) FROM tn GROUP BY c ORDER BY c;
SELECT DISTINCT c FROM tn ORDER BY c;
CREATE UNIQUE INDEX tn_u ON tn (c);
CREATE TABLE tc (id int, c ct, k cc);
INSERT INTO tc VALUES (1,ROW('a',1),ROW('a',1)),(2,ROW('B',1),ROW('B',1)),(3,ROW('b',1),ROW('b',1)),(4,ROW('A',2),ROW('A',2)),(5,ROW('Z',0),ROW('Z',0));
SELECT string_agg(id::text, ',' ORDER BY c), string_agg(id::text, ',' ORDER BY k) FROM tc;
SELECT min(c), max(c), min(k), max(k), greatest(ROW('a',1)::ct, ROW('B',1)::ct) FROM tc;
SELECT (c).a < 'b' AS icu, (k).a < 'b' AS c, id FROM tc ORDER BY id;
CREATE INDEX tc_c ON tc (c);
CREATE INDEX tc_k ON tc (k);
SET enable_seqscan = off;
SELECT id FROM tc WHERE c >= ROW('Z',0)::ct;
SELECT id FROM tc WHERE c < ROW('b',0)::ct ORDER BY c;
SELECT id FROM tc ORDER BY c DESC LIMIT 2;
SELECT id FROM tc WHERE k < ROW('b',0)::cc ORDER BY k;
RESET enable_seqscan;
SELECT id, rank() OVER (ORDER BY c) FROM tc ORDER BY 2, 1;
SELECT ARRAY(SELECT c FROM tc ORDER BY c)::text;
SELECT id FROM tc ORDER BY ROW((c).a, id);
SELECT id FROM tc t ORDER BY t;
SELECT array_agg(id ORDER BY ARRAY[c]) FROM tc;
