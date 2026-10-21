-- FROM subqueries Postgres plans as query levels of their own (ORDER BY, LIMIT,
-- DISTINCT, grouping, set-returning outputs): a Subquery Scan when it isn't trivial, the
-- subquery's order offered to the query above, clauses kept out of it when unsafe.
CREATE TABLE gc2_t (x int, y int, z int);
INSERT INTO gc2_t SELECT i % 10, i % 10, i % 10 FROM generate_series(1, 1000) i;
CREATE INDEX gc2_t_xy ON gc2_t (x, y);
ANALYZE gc2_t;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF)
SELECT DISTINCT ON (y, x) x, y FROM (SELECT * FROM gc2_t ORDER BY x) s;
EXPLAIN (COSTS OFF)
SELECT DISTINCT ON (y, x) x, y FROM (SELECT * FROM gc2_t ORDER BY x, z, y) s ORDER BY y, x, z;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT x, y FROM (SELECT * FROM gc2_t ORDER BY x, z, y) s ORDER BY x, y, z;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT x, y, z FROM (SELECT * FROM gc2_t ORDER BY x, z, y) s ORDER BY x, y, z;
RESET enable_hashagg;
EXPLAIN (COSTS OFF)
SELECT DISTINCT y, x FROM (SELECT * FROM gc2_t ORDER BY x) s;
SELECT DISTINCT ON (y, x) x, y FROM (SELECT * FROM gc2_t ORDER BY x) s;

CREATE TABLE gc2_lim (pk int PRIMARY KEY, c1 int, c2 int);
INSERT INTO gc2_lim VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3), (4, 4, 4), (5, 1, 1), (6, 2, 2), (7, 3, 3), (8, 4, 4);
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT pk, c2 FROM gc2_lim ORDER BY c1, pk) AS x LIMIT 3;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT pk, c1, c2 FROM gc2_lim ORDER BY c1, pk) AS x LIMIT 3;
EXPLAIN (COSTS OFF) SELECT pk FROM (SELECT pk, c1 FROM gc2_lim ORDER BY c1) s;
EXPLAIN (COSTS OFF, VERBOSE) SELECT pk FROM (SELECT pk, c1 FROM gc2_lim ORDER BY c1) s;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT pk, c1 FROM gc2_lim ORDER BY c1 LIMIT 5) s WHERE pk > 2;
SELECT * FROM (SELECT pk, c2 FROM gc2_lim ORDER BY c1, pk) AS x LIMIT 3;
SELECT * FROM (SELECT pk, c1 FROM gc2_lim ORDER BY c1 LIMIT 5) s WHERE pk > 2 ORDER BY pk;

-- Set-returning and volatile outputs keep the clauses that read them (or are volatile)
-- above the subquery.
CREATE FUNCTION gc2_tattle(x int, y int) RETURNS bool VOLATILE LANGUAGE plpgsql AS
$$ BEGIN RAISE NOTICE 'x = %, y = %', x, y; RETURN x > y; END $$;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 9 AS x, unnest(array[1,2,3,11,12,13]) AS u) ss WHERE gc2_tattle(x, 8);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 9 AS x, unnest(array[1,2,3,11,12,13]) AS u) ss WHERE gc2_tattle(x, u);
SELECT * FROM (SELECT 9 AS x, unnest(array[1,2,3,11,12,13]) AS u) ss WHERE gc2_tattle(x, u);

-- An operator of another btree family than the grouping's stays above it.
CREATE TYPE gc2_rec AS (x numeric);
CREATE TABLE gc2_pdt (id int, a gc2_rec);
INSERT INTO gc2_pdt VALUES (1, ROW(1.00)::gc2_rec), (2, ROW(1.0)::gc2_rec), (3, ROW(2)::gc2_rec);
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT DISTINCT ON (a) id, a FROM gc2_pdt ORDER BY a, id) s WHERE a *= ROW(1.0)::gc2_rec;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT DISTINCT a FROM gc2_pdt) s WHERE a *= ROW(1.0)::gc2_rec;
SELECT * FROM (SELECT DISTINCT ON (a) id, a FROM gc2_pdt ORDER BY a, id) s WHERE a *= ROW(1.0)::gc2_rec;

-- LATERAL references from inside a parenthesized join to FROM items left of it.
SELECT count(*) FROM gc2_lim a, gc2_lim b JOIN LATERAL (VALUES (a.pk)) ss(v) ON b.pk = ss.v;
SELECT * FROM gc2_lim a, gc2_lim b LEFT JOIN LATERAL (SELECT a.c1 FROM gc2_lim c WHERE c.pk = 1) ss(z)
  ON b.c2 = ss.z
  WHERE a.pk < 3 AND b.pk < 4 ORDER BY a.pk, b.pk;
SELECT * FROM gc2_lim a, (gc2_lim b JOIN (gc2_lim c JOIN LATERAL (SELECT a.pk + b.pk + c.pk AS s) ss ON true) ON true)
  WHERE a.pk = 1 AND b.pk = 2 ORDER BY c.pk;
SELECT * FROM gc2_lim x CROSS JOIN (gc2_lim x CROSS JOIN LATERAL (SELECT x.c1) ss);
