-- Composite values compare field by field under each field's collation (record_cmp's
-- attcollation): a CREATE TYPE's COLLATE, a table's column's, an anonymous ROW()'s field
-- expressions'. Operators, ORDER BY, min / max, GREATEST, IN / ANY, window ORDER BY and
-- PARTITION BY, GROUP BY, DISTINCT, count(DISTINCT), unique indexes, arrays, nested
-- composites, domains and range types over them, and btree indexes on such columns
-- (index key format 3). A field access derives the field's collation.
CREATE COLLATION a75ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TYPE a75ct AS (a text COLLATE "en-x-icu", b int);
CREATE TYPE a75cn AS (a text COLLATE a75ci, b int);
CREATE TYPE a75nest AS (x a75ct, y text COLLATE "en-x-icu");
CREATE TYPE a75r AS RANGE (subtype = a75ct);
CREATE DOMAIN a75dct AS a75ct CHECK ((VALUE).b >= 0);
SELECT ROW('a',1)::a75ct < ROW('B',1)::a75ct AS named, ROW('a',1) < ROW('B',1) AS rowcmp;
SELECT r1 < r2 AS anon FROM (SELECT ROW('a'::text COLLATE "en-x-icu",1) r1, ROW('B'::text COLLATE "en-x-icu",1) r2) s;
SELECT r1 < r2 AS anon_c FROM (SELECT ROW('a'::text,1) r1, ROW('B'::text,1) r2) s;
SELECT s1 < s2 AS wholerow FROM (SELECT 'a'::text COLLATE "en-x-icu" AS x) s1, (SELECT 'B'::text COLLATE "en-x-icu" AS x) s2;
SELECT ARRAY[ROW('a',1)::a75ct] < ARRAY[ROW('B',1)::a75ct] AS arr;
SELECT ROW(ROW('a',1)::a75ct, 'x')::a75nest < ROW(ROW('B',1)::a75ct, 'x')::a75nest AS nest,
       ROW(ROW('a',1)::a75ct, 'b')::a75nest < ROW(ROW('a',1)::a75ct, 'B')::a75nest AS nest2;
SELECT a75r(ROW('a',1)::a75ct, ROW('B',1)::a75ct) AS rng;
SELECT a75r(ROW('a',1)::a75ct, ROW('c',1)::a75ct) @> ROW('B',1)::a75ct AS rng_contains;
SELECT a75r(ROW('b',1)::a75ct, ROW('B',1)::a75ct);
SELECT ROW('a',1)::a75dct < ROW('B',1)::a75dct AS dom;
SELECT ROW('a',1)::a75cn = ROW('A',1)::a75cn AS cn_eq, ROW('a',1)::a75cn < ROW('B',1)::a75cn AS cn_lt;
CREATE TABLE a75tc (id int, c a75ct, d a75dct, n a75cn, ca a75ct[]);
INSERT INTO a75tc VALUES
  (1, ROW('a',1), ROW('a',1), ROW('a',1), ARRAY[ROW('a',1)::a75ct]),
  (2, ROW('B',1), ROW('B',1), ROW('A',1), ARRAY[ROW('B',1)::a75ct]),
  (3, ROW('b',1), ROW('b',1), ROW('b',1), ARRAY[ROW('b',1)::a75ct]),
  (4, ROW('A',2), ROW('A',2), ROW('B',2), ARRAY[ROW('A',2)::a75ct]),
  (5, ROW('Z',0), ROW('Z',0), NULL, NULL),
  (6, NULL, NULL, ROW(NULL,1), ARRAY[NULL::a75ct]),
  (7, ROW(NULL,1), ROW(NULL,1), ROW('a',NULL), ARRAY[ROW(NULL,1)::a75ct]);
SELECT count(*) FROM a75tc WHERE c >= ROW('Z',0)::a75ct;
SELECT min(c), max(c), min(d), max(d), min(ca), max(ca) FROM a75tc;
SELECT string_agg(id::text, ',' ORDER BY c), string_agg(id::text, ',' ORDER BY d DESC), string_agg(id::text, ',' ORDER BY ca) FROM a75tc;
SELECT id FROM a75tc ORDER BY ca NULLS FIRST, id;
SELECT count(DISTINCT n), count(DISTINCT c) FROM a75tc;
SELECT n, count(*) FROM a75tc GROUP BY n ORDER BY n;
SELECT DISTINCT n FROM a75tc ORDER BY n;
SELECT id, rank() OVER (ORDER BY c) FROM a75tc ORDER BY 2, 1;
SELECT id, first_value(id) OVER (PARTITION BY n ORDER BY id) FROM a75tc ORDER BY 1;
SELECT greatest(ROW('a',1)::a75ct, ROW('B',1)::a75ct), least(ROW('a',1)::a75ct, ROW('B',1)::a75ct);
SELECT id FROM a75tc WHERE c IN (ROW('B',1)::a75ct, ROW('z',0)::a75ct) ORDER BY 1;
SELECT id FROM a75tc WHERE c > ALL (ARRAY[ROW('a',1)::a75ct, ROW('A',2)::a75ct]) ORDER BY 1;
SELECT a.id, b.id FROM a75tc a JOIN a75tc b ON a.c < b.c AND b.id = 2 ORDER BY 1;
SELECT ((c).a, (c).b) < ('b', 0) AS rc, (c).a < 'b' AS field, id FROM a75tc ORDER BY id;
SELECT array_agg(c ORDER BY c)::text FROM a75tc;
SELECT id FROM a75tc ORDER BY ROW((c).a, id);
SELECT id FROM a75tc t ORDER BY t;
CREATE INDEX a75tc_c ON a75tc (c);
CREATE INDEX a75tc_ca ON a75tc (ca);
CREATE INDEX a75tc_d ON a75tc (d DESC);
SET enable_seqscan = off;
SELECT id FROM a75tc WHERE c >= ROW('Z',0)::a75ct;
SELECT id FROM a75tc WHERE c < ROW('b',0)::a75ct ORDER BY c;
SELECT id FROM a75tc WHERE c > ROW('a',5)::a75ct AND c <= ROW('b',1)::a75ct ORDER BY c DESC;
SELECT id FROM a75tc ORDER BY c DESC LIMIT 3;
SELECT id FROM a75tc ORDER BY c NULLS FIRST LIMIT 3;
SELECT id FROM a75tc WHERE ca > ARRAY[ROW('a',9)::a75ct] ORDER BY ca;
SELECT min(c), max(c) FROM a75tc;
SELECT id FROM a75tc WHERE d < ROW('b',0)::a75dct ORDER BY d;
RESET enable_seqscan;
CREATE UNIQUE INDEX a75tc_n ON a75tc (n);
CREATE TABLE a75tu (n a75cn UNIQUE);
INSERT INTO a75tu VALUES (ROW('x',1));
INSERT INTO a75tu VALUES (ROW('X',1));
INSERT INTO a75tu VALUES (ROW('X',1)) ON CONFLICT (n) DO NOTHING;
SELECT * FROM a75tu;
CREATE TABLE a75tr (r a75r);
INSERT INTO a75tr VALUES (a75r(ROW('a',1)::a75ct, ROW('c',1)::a75ct)), (a75r(ROW('B',1)::a75ct, ROW('z',1)::a75ct));
SELECT r FROM a75tr ORDER BY r;
SELECT r FROM a75tr WHERE r @> ROW('b',5)::a75ct ORDER BY r;
-- min / max of a composite aren't planned as index scans (no sort operator): a row with a
-- NULL field is a value all the same.
CREATE TYPE a75p AS (a text, b int);
CREATE TABLE a75m (id int, c a75p);
INSERT INTO a75m VALUES (1, ROW('Z',0)), (2, ROW(NULL,1)), (3, NULL), (4, ROW('a',1));
CREATE INDEX a75m_c ON a75m (c);
SET enable_seqscan = off;
SELECT max(c), min(c) FROM a75m;
EXPLAIN (COSTS OFF) SELECT max(c) FROM a75m;
RESET enable_seqscan;
DROP TABLE a75m, a75tc, a75tu, a75tr;
DROP TYPE a75p, a75nest, a75r;
DROP DOMAIN a75dct;
DROP TYPE a75ct, a75cn;
DROP COLLATION a75ci;
