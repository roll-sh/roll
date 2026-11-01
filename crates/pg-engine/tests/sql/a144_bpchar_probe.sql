-- char(n) equality probes bound btree and hash scans (agent 144): the probe is trimmed of
-- trailing spaces and padded to n, as the key is stored, and a probe longer than n finds
-- nothing (bpchareq). Short, padded and over-long probes, `= ANY` / IN, varchar and text
-- probes of a char(n) column and the reverse, typmod'd constants in EXPLAIN, multi-column
-- keys, a DESC key, a hash index, a domain over char(n), ALTER COLUMN TYPE, prepared
-- statements, nested-loop parameters, own writes, citext and name keys.
CREATE TABLE a144_c (id int PRIMARY KEY, c char(5), u bpchar, k int);
INSERT INTO a144_c SELECT g, CASE WHEN g % 7 = 0 THEN 'AB' ELSE lpad(g::text, 5, 'x') END,
  'q' || g, g % 3 FROM generate_series(1, 2000) g;
INSERT INTO a144_c VALUES (3001, 'ab' || chr(9), 'z', 0), (3002, 'É', 'É  ', 1),
  (3003, NULL, NULL, 2), (3004, '', '', 0), (3005, ' A', ' A', 1);
CREATE INDEX a144_c_c ON a144_c (c);
CREATE INDEX a144_c_u ON a144_c (u);
CREATE INDEX a144_c_kc ON a144_c (k, c DESC);
CREATE TABLE a144_v (id int PRIMARY KEY, v varchar(10), t text);
INSERT INTO a144_v SELECT g, 'AB' || (g % 3), 'AB' || (g % 3) FROM generate_series(1, 300) g;
INSERT INTO a144_v VALUES (301, 'AB', 'AB'), (302, 'AB  ', 'AB  ');
CREATE INDEX a144_v_v ON a144_v (v);
CREATE INDEX a144_v_t ON a144_v (t);
ANALYZE a144_c;
ANALYZE a144_v;
SET enable_seqscan = off;
SELECT count(*) FROM a144_c WHERE c = 'AB';
SELECT count(*) FROM a144_c WHERE c = 'AB   ';
SELECT count(*) FROM a144_c WHERE c = 'AB      ';
SELECT count(*) FROM a144_c WHERE c = 'ABCDEFG';
SELECT count(*) FROM a144_c WHERE c = 'ABCDEF ';
SELECT count(*) FROM a144_c WHERE c = '';
SELECT count(*) FROM a144_c WHERE c = '   ';
SELECT id FROM a144_c WHERE c = ' A';
SELECT id FROM a144_c WHERE c = 'x0007';
SELECT id FROM a144_c WHERE c = 'x0008' ORDER BY id;
SELECT count(*) FROM a144_c WHERE c = ANY ('{AB,x0008,"x0009  ",TOOLONGX,NULL}');
SELECT id FROM a144_c WHERE c IN ('x0010', 'x0011  ', 'x00123') ORDER BY id;
SELECT count(*) FROM a144_c WHERE c = 'AB'::varchar;
SELECT count(*) FROM a144_c WHERE c = 'AB'::text;
SELECT count(*) FROM a144_c WHERE c = 'AB'::char(3);
SELECT count(*) FROM a144_c WHERE c = 'AB'::char(10);
SELECT id FROM a144_c WHERE c = 'É';
SELECT id FROM a144_c WHERE c = 'ab' || chr(9);
SELECT id FROM a144_c WHERE c = 'ab';
SELECT id FROM a144_c WHERE u = 'É';
SELECT id FROM a144_c WHERE u = 'q7  ';
SELECT id FROM a144_c WHERE c IS NULL;
SELECT id, c FROM a144_c WHERE k = 0 AND c = 'x0003' ORDER BY id;
SELECT count(*) FROM a144_c WHERE k = 1 AND c = 'AB ';
SELECT count(*) FROM a144_c WHERE k = 1 AND c = ANY ('{AB,x0010}');
SELECT id FROM a144_c WHERE c > 'x1997' ORDER BY c, id;
SELECT id FROM a144_c WHERE c BETWEEN 'x0999' AND 'x1001  ' ORDER BY c, id;
SELECT id FROM a144_c WHERE c < 'AB' ORDER BY c, id;
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = 'AB';
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = 'ABCDEFG';
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = ANY ('{AB,x0008}');
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = 'AB'::varchar;
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = 'AB'::text;
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = 'AB'::char(5);
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE c = 'AB'::char(3);
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_c WHERE u = 'q7  ';
EXPLAIN (COSTS OFF) SELECT id FROM a144_c WHERE k = 1 AND c = 'AB ';
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT count(*) FROM a144_c WHERE c = 'ZZZZZ';
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT count(*) FROM a144_c WHERE c = 'ZZZZZZ';
-- The hash index.
SET enable_bitmapscan = off;
SET enable_indexonlyscan = off;
DROP INDEX a144_c_c, a144_c_kc;
CREATE INDEX a144_c_h ON a144_c USING hash (c);
EXPLAIN (COSTS OFF) SELECT id FROM a144_c WHERE c = 'x0008';
SELECT id FROM a144_c WHERE c = 'x0008';
SELECT id FROM a144_c WHERE c = 'x0008   ';
SELECT id FROM a144_c WHERE c = 'x00088';
SELECT count(*) FROM a144_c WHERE c = 'AB';
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT count(*) FROM a144_c WHERE c = 'ZZZZZZ';
RESET enable_bitmapscan;
RESET enable_indexonlyscan;
DROP INDEX a144_c_h;
CREATE INDEX a144_c_c ON a144_c (c);
-- varchar and text columns probed by char(n), text and padded values.
SELECT count(*) FROM a144_v WHERE v = 'AB'::char(5);
SELECT count(*) FROM a144_v WHERE t = 'AB'::char(5);
SELECT count(*) FROM a144_v WHERE v = 'AB  ';
SELECT count(*) FROM a144_v WHERE v = 'AB'::text;
SELECT count(*) FROM a144_v WHERE t = 'AB  '::varchar;
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_v WHERE v = 'AB'::char(5);
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_v WHERE t = 'AB'::char(5);
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_v WHERE v = 'AB'::text;
-- Prepared statements: bpchar, text and varchar parameters.
PREPARE a144_p1 (bpchar) AS SELECT count(*) FROM a144_c WHERE c = $1;
EXECUTE a144_p1 ('AB');
EXECUTE a144_p1 ('AB     ');
EXECUTE a144_p1 ('ABCDEFGH');
EXECUTE a144_p1 ('x0008');
PREPARE a144_p2 (text) AS SELECT count(*) FROM a144_c WHERE c = $1;
EXECUTE a144_p2 ('AB');
EXECUTE a144_p2 ('AB   ');
PREPARE a144_p3 (varchar) AS SELECT count(*) FROM a144_c WHERE c = ANY (ARRAY[$1, 'x0009']);
EXECUTE a144_p3 ('AB');
EXECUTE a144_p3 ('AB   ');
SET plan_cache_mode = force_generic_plan;
EXECUTE a144_p1 ('AB  ');
EXECUTE a144_p1 ('ABCDEFG');
EXPLAIN (COSTS OFF) EXECUTE a144_p1 ('AB');
RESET plan_cache_mode;
DEALLOCATE ALL;
-- Joins: a char(5) column probed by another table's char(3) / char(8) / bpchar values.
CREATE TABLE a144_o (n int, c3 char(3), c8 char(8), b bpchar);
INSERT INTO a144_o VALUES (1, 'AB', 'AB', 'AB'), (2, 'x0', 'x0008   ', 'x0008  '),
  (3, NULL, 'TOOLONGX', 'TOOLONG'), (4, 'É', 'É', 'É');
ANALYZE a144_o;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SELECT o.n, count(c.id) FROM a144_o o LEFT JOIN a144_c c ON c.c = o.c3 GROUP BY o.n ORDER BY o.n;
SELECT o.n, count(c.id) FROM a144_o o LEFT JOIN a144_c c ON c.c = o.c8 GROUP BY o.n ORDER BY o.n;
SELECT o.n, count(c.id) FROM a144_o o LEFT JOIN a144_c c ON c.c = o.b GROUP BY o.n ORDER BY o.n;
SELECT o.n, (SELECT count(*) FROM a144_c c WHERE c.c = o.c8) FROM a144_o o ORDER BY o.n;
EXPLAIN (COSTS OFF) SELECT o.n, c.id FROM a144_o o JOIN a144_c c ON c.c = o.c8;
RESET enable_hashjoin;
RESET enable_mergejoin;
-- The transaction's own writes.
BEGIN;
INSERT INTO a144_c VALUES (4001, 'NEW', 'NEW', 1);
UPDATE a144_c SET c = 'AB  ' WHERE id = 1;
SELECT id FROM a144_c WHERE c = 'NEW  ' ORDER BY id;
SELECT count(*) FROM a144_c WHERE c = 'AB';
ROLLBACK;
-- A domain over char(n), and ALTER COLUMN TYPE.
CREATE DOMAIN a144_d AS char(4);
CREATE TABLE a144_dt (id int, d a144_d);
INSERT INTO a144_dt SELECT g, 'd' || (g % 50) FROM generate_series(1, 500) g;
CREATE INDEX ON a144_dt (d);
ANALYZE a144_dt;
SELECT count(*) FROM a144_dt WHERE d = 'd7';
SELECT count(*) FROM a144_dt WHERE d = 'd7    ';
SELECT count(*) FROM a144_dt WHERE d = 'd7xyz';
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_dt WHERE d::bpchar = 'd7';
ALTER TABLE a144_dt ALTER COLUMN d TYPE char(8);
SELECT count(*) FROM a144_dt WHERE d = 'd7';
SELECT count(*) FROM a144_dt WHERE d = 'd7xyz';
ALTER TABLE a144_dt ALTER COLUMN d TYPE char(3);
SELECT count(*) FROM a144_dt WHERE d = 'd7';
SELECT count(*) FROM a144_dt WHERE d = 'd7  ';
SELECT count(*) FROM a144_dt WHERE d = 'd7xy';
ALTER TABLE a144_dt ALTER COLUMN d TYPE varchar(6);
SELECT count(*) FROM a144_dt WHERE d = 'd7';
SELECT count(*) FROM a144_dt WHERE d = 'd7 ';
-- name and citext keys.
CREATE TABLE a144_n (id int, n name);
INSERT INTO a144_n SELECT g, 'n' || g FROM generate_series(1, 500) g;
CREATE INDEX ON a144_n (n);
ANALYZE a144_n;
SELECT id FROM a144_n WHERE n = 'n42';
SELECT id FROM a144_n WHERE n = 'n42'::text;
SELECT id FROM a144_n WHERE n = 'n42'::varchar;
EXPLAIN (COSTS OFF) SELECT id FROM a144_n WHERE n = 'n42'::text;
CREATE EXTENSION IF NOT EXISTS citext;
CREATE TABLE a144_ci (id int, c citext);
INSERT INTO a144_ci SELECT g, CASE WHEN g % 2 = 0 THEN 'Key' ELSE 'key' END || (g % 100)
  FROM generate_series(1, 1000) g;
CREATE INDEX ON a144_ci (c);
ANALYZE a144_ci;
SELECT count(*) FROM a144_ci WHERE c = 'KEY7';
SELECT count(*) FROM a144_ci WHERE c = 'key7';
SELECT count(*) FROM a144_ci WHERE c = ANY ('{KEY7,kEy8,none}'::citext[]);
SELECT count(*) FROM a144_ci WHERE c = 'key7'::text;
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_ci WHERE c = 'KEY7';
EXPLAIN (COSTS OFF) SELECT count(*) FROM a144_ci WHERE c = 'key7'::text;
RESET enable_seqscan;
DROP TABLE a144_c, a144_v, a144_o, a144_dt, a144_n, a144_ci;
DROP DOMAIN a144_d;
DROP EXTENSION citext;
