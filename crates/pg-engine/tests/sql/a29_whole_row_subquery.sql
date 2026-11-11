-- A FROM subquery's whole row when the subquery isn't pulled up: a Var to its
-- Subquery Scan (`s.*`), its IS [NOT] NULL test kept whole.
CREATE TABLE a29_wt (a int, b text);
INSERT INTO a29_wt VALUES (1, 'x'), (2, 'y'), (NULL, NULL), (3, NULL);
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT a, b FROM a29_wt OFFSET 0) s;
SELECT s FROM (SELECT a, b FROM a29_wt OFFSET 0) s;
EXPLAIN (verbose, costs off) SELECT s, s.a FROM (SELECT a, b FROM a29_wt OFFSET 0) s;
EXPLAIN (verbose, costs off) SELECT s.* FROM (SELECT a, b FROM a29_wt OFFSET 0) s;
EXPLAIN (verbose, costs off) SELECT s IS NULL, s IS NOT NULL FROM (SELECT a, b FROM a29_wt OFFSET 0) s;
SELECT s IS NULL, s IS NOT NULL FROM (SELECT a, b FROM a29_wt OFFSET 0) s;
SELECT a, b FROM (SELECT a, b FROM a29_wt OFFSET 0) s WHERE s IS NULL;
SELECT a, b FROM (SELECT a, b FROM a29_wt OFFSET 0) s WHERE s IS NOT NULL;
EXPLAIN (verbose, costs off) SELECT row_to_json(s) FROM (SELECT a, b || '!' AS c FROM a29_wt OFFSET 0) s;
SELECT row_to_json(s) FROM (SELECT a, b || '!' AS c FROM a29_wt OFFSET 0) s;
-- (Pulled up: a ROW() of the subquery's outputs.)
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT a, b FROM a29_wt) s;
EXPLAIN (verbose, costs off) SELECT s IS NULL FROM (SELECT a, b FROM a29_wt) s;
SELECT s IS NULL FROM (SELECT a, b FROM a29_wt) s;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT a, b FROM a29_wt LIMIT 1) s;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT a, b FROM a29_wt ORDER BY a OFFSET 0) s;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT DISTINCT a FROM a29_wt) s;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT a, count(*) FROM a29_wt GROUP BY a) s;
SELECT s FROM (SELECT a, count(*) FROM a29_wt GROUP BY a) s ORDER BY 1;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT b, a FROM a29_wt OFFSET 0) s;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT * FROM a29_wt OFFSET 0) s;
EXPLAIN (verbose, costs off) SELECT s FROM (SELECT a FROM a29_wt OFFSET 0) s;
DROP TABLE a29_wt;
