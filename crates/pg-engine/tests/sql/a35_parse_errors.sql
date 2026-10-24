-- Found by the differential tester (tests/difftest.rs): parse-analysis errors.
CREATE TABLE t0 (id int PRIMARY KEY, a text, b int);
CREATE TABLE t1 (id int, c text);
CREATE TABLE t2 (id int, d text) PARTITION BY LIST (d);
CREATE TABLE t2_p PARTITION OF t2 DEFAULT;
INSERT INTO t0 VALUES (1, 'x', 1), (2, 'y', 2);
INSERT INTO t1 VALUES (1, 'p'), (3, 'q');
-- Locking the nullable side of an outer join (make_outerjoininfo), after outer joins a
-- strict qual reduced.
SELECT 1 FROM t0 LEFT JOIN t1 ON t0.id = t1.id FOR UPDATE;
SELECT 1 FROM t0 RIGHT JOIN t1 ON t0.id = t1.id FOR SHARE;
SELECT 1 FROM t0 FULL JOIN t1 ON t0.id = t1.id FOR KEY SHARE;
SELECT 1 FROM t0 FULL JOIN t2 ON t0.id = t2.id FOR NO KEY UPDATE;
SELECT 1 FROM t0 LEFT JOIN t1 ON t0.id = t1.id FOR UPDATE OF t0;
SELECT t0.id FROM t0 LEFT JOIN t1 ON t0.id = t1.id WHERE t1.c = 'p' FOR UPDATE;
SELECT 1 FROM (t0 LEFT JOIN t1 ON t0.id = t1.id) CROSS JOIN (SELECT 1) s FOR UPDATE;
SELECT 1 FROM t0 x, (SELECT t0.id FROM t0 LEFT JOIN t1 ON t0.id = t1.id) s WHERE s.id = x.id FOR UPDATE;
SELECT row_number() OVER () FROM t0 FOR UPDATE;
SELECT generate_series(1, 2) FROM t0 FOR SHARE;
-- Polymorphic parameters with only untyped literals.
SELECT to_json('Ab');
SELECT to_jsonb('Ab');
SELECT to_json('Ab'::text);
SELECT array_length('{1}', 1);
SELECT coalesce(to_jsonb(NULL::int), '1');
-- A sub-select's ungrouped outer references, also when the sub-select folds away.
SELECT count(*), (SELECT t0.b FROM t1 LIMIT 1) FROM t0;
SELECT count(*) FROM t0 HAVING EXISTS (SELECT t0.b FROM t1);
SELECT count(*), (SELECT 1 FROM t1 WHERE t1.id = t0.b) FROM t0;
SELECT count(*), (WITH w AS (SELECT t0.a FROM t1) SELECT 1) FROM t0;
SELECT count(*) FROM t0 HAVING NULL BETWEEN (SELECT t0.b FROM t1 LIMIT 1) AND NULL;
SELECT count(*), CASE WHEN 5 IS NULL THEN (SELECT t0.a FROM t1 LIMIT 1) END FROM t0;
SELECT max(t0.a), (SELECT t0.b FROM t1 LIMIT 1) FROM t0 GROUP BY t0.id;
SELECT sum((SELECT t0.b)) FROM t0;
SELECT (SELECT count(t0.id) FROM t1) FROM t0;
SELECT (SELECT string_agg(t0.a, ',' ORDER BY t0.a) FROM t1) FROM t0;
-- DISTINCT ON and ORDER BY items match by target-list entry.
SELECT DISTINCT ON (1) b AS c0, 1 AS c1, b AS c2 FROM t0 ORDER BY 1, 2, 3;
SELECT DISTINCT ON (1) b AS c0, 1 AS c1, b AS c2 FROM t0 ORDER BY 1, 2, b;
SELECT DISTINCT ON (3) b AS c0, 1 AS c1, b AS c2 FROM t0 ORDER BY 1, 3;
SELECT DISTINCT ON (b) b, id FROM t0 ORDER BY id, b;
DROP TABLE t2;
