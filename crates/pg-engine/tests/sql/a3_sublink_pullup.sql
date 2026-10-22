-- IN sublinks as semi joins (pull_up_sublinks): `x IN (VALUES ...)` of constants as
-- `x = ANY ('{...}')` (convert_VALUES_to_ANY; not over a FROM subquery's VALUES or
-- non-constant items), other uncorrelated subqueries (VALUES lists, LIMIT / ORDER BY) as
-- semi joins, a correlated IN's own WHERE clauses before its comparison, sublinks in an
-- inner join's ON clause.
CREATE TABLE a3_sp_a (id int PRIMARY KEY, grp int, odd int, v text);
CREATE TABLE a3_sp_b (id int, grp int, odd int);
INSERT INTO a3_sp_a SELECT g, g % 100, g % 2, 'v' || g FROM generate_series(1, 1000) g;
INSERT INTO a3_sp_b SELECT g, g % 100, g % 2 FROM generate_series(1, 1000) g;
CREATE INDEX ON a3_sp_b (grp);
ANALYZE a3_sp_a;
ANALYZE a3_sp_b;
EXPLAIN (COSTS OFF) SELECT * FROM a3_sp_a WHERE id IN (VALUES (1), (2), (3));
SELECT * FROM a3_sp_a WHERE id IN (VALUES (1), (2), (3)) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT * FROM a3_sp_a WHERE 1.0::int IN ((VALUES (1), (3)));
EXPLAIN (COSTS OFF) SELECT * FROM a3_sp_a WHERE 2 IN ((VALUES (1), (3)));
SELECT count(*) FROM a3_sp_a WHERE 2 IN ((VALUES (1), (3)));
SELECT * FROM a3_sp_a WHERE id IN (SELECT x * x FROM (VALUES (30), (1)) AS x(x)) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT v FROM a3_sp_a WHERE id IN (VALUES (1), (2) LIMIT 1);
SELECT v FROM a3_sp_a WHERE id IN (VALUES (1), (2) LIMIT 1);
EXPLAIN (COSTS OFF) SELECT v FROM a3_sp_a WHERE id IN (VALUES (1), (2) OFFSET 1);
EXPLAIN (COSTS OFF) SELECT v FROM a3_sp_a WHERE id IN (VALUES (1), (2) ORDER BY 1);
SELECT v FROM a3_sp_a WHERE id IN (VALUES (2), (1) ORDER BY 1) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT * FROM a3_sp_a WHERE (id, odd) IN (VALUES (1, 1), (20, 0), (99, 9), (17, 99)) ORDER BY id;
SELECT * FROM a3_sp_a WHERE (id, odd) IN (VALUES (1, 1), (20, 0), (99, 9), (17, 99)) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT v FROM a3_sp_a WHERE id IN (VALUES (0), ((2 IN (SELECT (3)))::integer));
SELECT v FROM a3_sp_a WHERE id IN (VALUES (0), ((2 IN (SELECT (3)))::integer));
EXPLAIN (COSTS OFF)
SELECT * FROM a3_sp_a a WHERE grp IN (SELECT grp FROM a3_sp_b b WHERE b.odd = a.odd);
SELECT count(*) FROM a3_sp_a a WHERE grp IN (SELECT grp FROM a3_sp_b b WHERE b.odd = a.odd);
SELECT count(*) FROM a3_sp_a a JOIN a3_sp_b b ON a.grp IN (SELECT c.grp FROM a3_sp_b c WHERE c.odd = b.odd)
WHERE a.id < 50 AND b.id < 50;
SELECT count(*) FROM a3_sp_a a JOIN a3_sp_b b ON a.id = b.id AND NOT EXISTS (SELECT 1 FROM a3_sp_b c WHERE c.id = a.id + 1)
WHERE a.id < 50;
DROP TABLE a3_sp_a, a3_sp_b;
