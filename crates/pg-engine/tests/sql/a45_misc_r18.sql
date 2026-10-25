-- Found by the differential tester (tests/difftest.rs).
CREATE TABLE a (x int);
CREATE TABLE c (z int);
CREATE TABLE w0 (a bool, b int, d text);
CREATE TABLE w1 (c int);
INSERT INTO a VALUES (1), (2);
INSERT INTO c VALUES (1), (3);
INSERT INTO w0 VALUES (false, 6, ' x'), (NULL, 6, NULL);
INSERT INTO w1 VALUES (1);
-- An ON clause's reference to a FROM item outside its join: in the range table, not in its
-- namespace (errorMissingRTE's "invalid reference").
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT c.z = 1)) ON a1.x = c.z;
SELECT * FROM c JOIN (a a1 JOIN a a2 ON c.z = a1.x) ON true;
-- Pseudoconstant quals are tested once: no per-row cost, so they keep the order they were
-- distributed in (a pulled-up subquery's WHERE first), which decides which one fails.
EXPLAIN (COSTS OFF) SELECT * FROM w0 x WHERE EXISTS (SELECT count(*) FROM (SELECT 1 FROM w1 t2 WHERE int4range(x.b, 5) @> x.b) s1 WHERE x.a);
SELECT * FROM w0 x WHERE EXISTS (SELECT count(*) FROM (SELECT 1 FROM w1 t2 WHERE int4range(x.b, length(x.d) % 4) @> x.b) s1 WHERE x.a);
EXPLAIN (COSTS OFF) SELECT 1 FROM w1 WHERE (SELECT true) AND now() > '2000-01-01';
EXPLAIN (COSTS OFF) SELECT 1 FROM w1 WHERE now() > '2000-01-01' AND (SELECT true);
-- A cursor's query folds like any other.
CREATE VIEW v1 AS SELECT (32767::int2 + 4::int2) AS c0, 1 AS c1 FROM (VALUES (1), (2)) s1(v0);
BEGIN;
DECLARE a45c CURSOR FOR SELECT 1 AS c0, s2.o AS c2 FROM (v1 t1 LEFT JOIN unnest('{NULL,3}'::int4[]) WITH ORDINALITY s2 (u, o) ON false) WHERE NULL;
FETCH ALL FROM a45c;
COMMIT;
DROP VIEW v1;
