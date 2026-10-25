-- FROM subqueries whose outputs a LATERAL item reads are pulled up as their levels had
-- them before planning (pull_up_subqueries before the join search): the LATERAL item is
-- part of their join problem, a nested loop's PlaceHolderVar parameter computed by a
-- Result over a parameterized Append (create_nestloop_plan's change_plan_targetlist), and
-- a Limit shows its level's constants as constants (set_dummy_tlist_references).
CREATE TABLE a55_t (i int PRIMARY KEY);
CREATE TABLE a55_h (i int);

-- join 360's query: the appendrel member `select 0` under the parameterized Append, the
-- LATERAL item joined to it below the Limit, 13 rows for the join of the three.
EXPLAIN (VERBOSE)
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM
             (SELECT i FROM a55_t UNION ALL SELECT 0)
             JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
         JOIN (SELECT 3 AS y) ON i IS NOT NULL
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
-- The same over a plain table, a heap without an index, and without the inner join.
EXPLAIN (VERBOSE)
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM a55_t JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
         JOIN (SELECT 3 AS y) ON i IS NOT NULL
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
EXPLAIN (VERBOSE)
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM a55_h JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
         JOIN (SELECT 3 AS y) ON i IS NOT NULL
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
EXPLAIN (VERBOSE)
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM
             (SELECT i FROM a55_t UNION ALL SELECT 0)
             JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
EXPLAIN (VERBOSE)
SELECT * FROM
     (SELECT i, coalesce(i, j) AS k FROM
             (SELECT i FROM a55_t UNION ALL SELECT 0)
             JOIN (SELECT 1 AS j LIMIT 1) ON i = j) ss,
     LATERAL (SELECT k AS kl LIMIT 1);

-- Rows: empty tables (the right join's null-extended row), then with rows, NULLs in the
-- heap.
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM
             (SELECT i FROM a55_t UNION ALL SELECT 0)
             JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
INSERT INTO a55_t SELECT generate_series(-3, 5);
INSERT INTO a55_h VALUES (1), (NULL), (1), (2);
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM
             (SELECT i FROM a55_t UNION ALL SELECT 0)
             JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
         JOIN (SELECT 3 AS y) ON i IS NOT NULL
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM a55_h JOIN (SELECT 1 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
SELECT * FROM
     (SELECT k FROM
         (SELECT i, coalesce(i, j) AS k FROM a55_h JOIN (SELECT 5 AS j LIMIT 1) ON i = j)
         RIGHT JOIN (SELECT 2 AS x) ON true
     ),
     LATERAL (SELECT k AS kl LIMIT 1);
SELECT * FROM
     (SELECT i, coalesce(i, j) AS k FROM
             (SELECT i FROM a55_t UNION ALL SELECT 0)
             JOIN (SELECT 0 AS j LIMIT 1) ON i = j) ss,
     LATERAL (SELECT k AS kl, ss.i + 1 AS m LIMIT 1)
ORDER BY 1, 2;
SELECT * FROM
     (SELECT h.i, coalesce(h.i, t.i) AS k FROM a55_h h LEFT JOIN a55_t t ON h.i = t.i + 1) ss,
     LATERAL (SELECT k AS kl LIMIT 1)
ORDER BY 1, 2;

-- A Limit (and the Sort of its level under it) shows the select list's constants as they
-- are; a node above refers to them.
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT 1 AS j LIMIT 1) a, (SELECT 2 AS k LIMIT 1) b;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 1 AS j, x FROM generate_series(1, 3) x ORDER BY x LIMIT 2) s;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 1 AS j, x FROM generate_series(1, 3) x ORDER BY x LIMIT 2 OFFSET 1) s LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a55_h h JOIN (SELECT 1 AS j, 7 AS k LIMIT 1) b ON h.i = b.j;
SELECT * FROM (SELECT 1 AS j, x FROM generate_series(1, 3) x ORDER BY x LIMIT 2 OFFSET 1) s LIMIT 1;
SELECT * FROM a55_h h JOIN (SELECT 1 AS j, 7 AS k LIMIT 1) b ON h.i = b.j;

DROP TABLE a55_t, a55_h;
