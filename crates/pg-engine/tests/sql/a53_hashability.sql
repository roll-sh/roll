-- Hashing follows PostgreSQL's hashability: a type whose `=` doesn't hash (money, bit,
-- seg, an anonymous record, an array or composite of such) is grouped, de-duplicated and
-- matched by sorting; types that hash (inet, bpchar, arrays, ranges, jsonb, timetz) hash
-- joins. A sorted grouping returns its groups in its keys' order.
CREATE TABLE m (a money, b int, c bit(4));
INSERT INTO m SELECT (i % 7)::money, i, (i % 5)::bit(4) FROM generate_series(1, 50) i;
ANALYZE m;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM m GROUP BY a;
SELECT a, count(*) FROM m GROUP BY a;
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM m;
SELECT DISTINCT a FROM m;
EXPLAIN (COSTS OFF) SELECT c, count(*) FROM m GROUP BY c;
SELECT c, count(*) FROM m GROUP BY c;
EXPLAIN (COSTS OFF) SELECT a, b FROM m GROUP BY a, b;
EXPLAIN (COSTS OFF) SELECT a FROM m UNION SELECT a FROM m;
SELECT a FROM m UNION SELECT a FROM m;
SELECT a FROM m INTERSECT SELECT a FROM m;
SELECT a FROM m EXCEPT SELECT a FROM m WHERE b > 40;
EXPLAIN (COSTS OFF) SELECT b FROM m WHERE a NOT IN (SELECT a FROM m WHERE b < 3);
SELECT b FROM m WHERE a NOT IN (SELECT a FROM m WHERE b < 3);
EXPLAIN (COSTS OFF) SELECT ARRAY[a] FROM m GROUP BY 1;
SELECT ARRAY[a] FROM m GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT ROW(b % 3, b % 2) FROM m GROUP BY 1;
SELECT ROW(b % 3, b % 2) FROM m GROUP BY 1;
CREATE TYPE pt AS (p money, q int);
EXPLAIN (COSTS OFF) SELECT (a, b % 2)::pt FROM m GROUP BY 1;
SELECT count(*) FROM (SELECT (a, b % 2)::pt FROM m GROUP BY 1) s;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM m GROUP BY GROUPING SETS ((a), (b)) ORDER BY 1, 2 LIMIT 5;
WITH RECURSIVE r(a) AS (SELECT '1'::money UNION SELECT a + '1'::money FROM r WHERE a < '5'::money) SELECT * FROM r;
CREATE TABLE n (k int);
INSERT INTO n SELECT i % 5 FROM generate_series(1, 100) i;
ANALYZE n;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (COSTS OFF) SELECT * FROM n JOIN LATERAL (SELECT sum(b) FROM m WHERE m.a = n.k::money) s ON true;
RESET enable_hashjoin;
RESET enable_mergejoin;

-- a sorted grouping's groups come out in its sort order
CREATE TABLE g (a int, b int);
INSERT INTO g SELECT (i * 7919) % 13, i FROM generate_series(1, 50) i;
ANALYZE g;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM g GROUP BY a;
SELECT a, count(*) FROM g GROUP BY a;
RESET enable_hashagg;
SELECT a, count(*) FROM g GROUP BY a;
CREATE INDEX ON g (a);
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM g GROUP BY a;
SELECT a, count(*) FROM g GROUP BY a;
RESET enable_seqscan;

-- types that hash: hash joins
CREATE TABLE h1 (i inet, c char(4), v int[], tz timetz, j jsonb, r int4range);
INSERT INTO h1 SELECT ('10.0.0.' || (x % 20))::inet, (x % 20)::text, ARRAY[x % 20],
  ('10:00:00+0' || (x % 5))::timetz, jsonb_build_object('a', x % 20), int4range(x % 20, x % 20 + 1)
FROM generate_series(1, 2000) x;
CREATE TABLE h2 AS SELECT * FROM h1 WHERE upper(r) <= 10;
ANALYZE h1;
ANALYZE h2;
EXPLAIN (COSTS OFF) SELECT count(*) FROM h1 JOIN h2 USING (i);
SELECT count(*) FROM h1 JOIN h2 USING (i);
EXPLAIN (COSTS OFF) SELECT count(*) FROM h1 JOIN h2 USING (c);
SELECT count(*) FROM h1 JOIN h2 USING (c);
EXPLAIN (COSTS OFF) SELECT count(*) FROM h1 JOIN h2 USING (v);
SELECT count(*) FROM h1 JOIN h2 USING (v);
EXPLAIN (COSTS OFF) SELECT count(*) FROM h1 JOIN h2 USING (tz);
SELECT count(*) FROM h1 JOIN h2 USING (tz);
EXPLAIN (COSTS OFF) SELECT count(*) FROM h1 JOIN h2 USING (j);
SELECT count(*) FROM h1 JOIN h2 USING (j);
EXPLAIN (COSTS OFF) SELECT count(*) FROM h1 JOIN h2 USING (r);
SELECT count(*) FROM h1 JOIN h2 USING (r);
SELECT count(*) FROM (VALUES ('1.0'::jsonb), ('{"a": 1.0}'::jsonb)) a(j) JOIN (VALUES ('1.00'::jsonb), ('{"a": 1.000}'::jsonb)) b(j) ON a.j = b.j;
SELECT count(*) FROM (VALUES ('a'::char(2))) a(c) JOIN (VALUES ('a   '::char(4))) b(c) ON a.c = b.c;

-- an extension type without a hash operator class
CREATE EXTENSION seg;
CREATE TABLE s (x seg);
INSERT INTO s SELECT (i % 4 || ' .. ' || i % 4 + 1)::seg FROM generate_series(1, 20) i;
ANALYZE s;
EXPLAIN (COSTS OFF) SELECT x, count(*) FROM s GROUP BY x;
SELECT x, count(*) FROM s GROUP BY x;
SELECT DISTINCT x FROM s;
DROP EXTENSION seg CASCADE;
