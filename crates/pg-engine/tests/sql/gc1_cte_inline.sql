-- CTE inlining (group C1): a non-recursive SELECT CTE without side effects, read once
-- (or NOT MATERIALIZED), is planned as a subquery in place of its reference; else it is
-- a CTE Scan of its InitPlan.
CREATE TABLE gci_t (a int, b int);
INSERT INTO gci_t VALUES (0, 5), (1, 6), (2, 7);
ANALYZE gci_t;
-- Inlined: an unread column isn't computed (1/0 raises nothing).
WITH w AS (SELECT 1 / a AS x, b AS y FROM gci_t) SELECT y FROM w ORDER BY y;
WITH w AS MATERIALIZED (SELECT 1 / a AS x, b AS y FROM gci_t) SELECT y FROM w;
EXPLAIN (COSTS OFF) WITH w AS (SELECT a, b FROM gci_t) SELECT * FROM w WHERE a = 1;
WITH w AS (SELECT a, b FROM gci_t) SELECT * FROM w WHERE a = 1;
EXPLAIN (COSTS OFF) WITH w AS MATERIALIZED (SELECT a, b FROM gci_t) SELECT * FROM w WHERE a = 1;
-- Read twice: a CTE, unless NOT MATERIALIZED.
EXPLAIN (COSTS OFF)
WITH w AS (SELECT a, b FROM gci_t) SELECT * FROM w w1, w w2 WHERE w1.a = w2.a;
EXPLAIN (COSTS OFF)
WITH w AS NOT MATERIALIZED (SELECT a, b FROM gci_t) SELECT * FROM w w1, w w2 WHERE w1.a = w2.a;
WITH w AS NOT MATERIALIZED (SELECT a, b FROM gci_t) SELECT * FROM w w1, w w2 WHERE w1.a = w2.a ORDER BY 1;
-- Volatile: a CTE.
EXPLAIN (COSTS OFF) WITH w AS (SELECT a, random() AS r FROM gci_t) SELECT a FROM w WHERE a = 1;
-- FOR UPDATE: a CTE.
EXPLAIN (COSTS OFF) WITH w AS (SELECT a FROM gci_t FOR UPDATE) SELECT a FROM w WHERE a = 1;
-- One CTE reading another, each read once.
EXPLAIN (COSTS OFF)
WITH w AS (SELECT a, b FROM gci_t), w2 AS (SELECT a + 1 AS c, b FROM w) SELECT * FROM w2 WHERE b > 5;
WITH w AS (SELECT a, b FROM gci_t), w2 AS (SELECT a + 1 AS c, b FROM w) SELECT * FROM w2 WHERE b > 5 ORDER BY 1;
-- Column aliases; a reference in a sub-select.
EXPLAIN (COSTS OFF)
WITH w(x, y) AS (SELECT a, b FROM gci_t) SELECT * FROM gci_t WHERE a IN (SELECT x FROM w WHERE y > 5);
WITH w(x, y) AS (SELECT a, b FROM gci_t) SELECT * FROM gci_t WHERE a IN (SELECT x FROM w WHERE y > 5) ORDER BY 1;
-- In DML.
EXPLAIN (COSTS OFF)
WITH w AS (SELECT a FROM gci_t WHERE b = 6) UPDATE gci_t SET b = b + 10 FROM w WHERE gci_t.a = w.a;
WITH w AS (SELECT a FROM gci_t WHERE b = 6) UPDATE gci_t SET b = b + 10 FROM w WHERE gci_t.a = w.a RETURNING gci_t.*;
-- A data-modifying CTE is never inlined.
EXPLAIN (COSTS OFF)
WITH d AS (DELETE FROM gci_t WHERE a = 2 RETURNING a) SELECT * FROM d;
WITH d AS (DELETE FROM gci_t WHERE a = 2 RETURNING a) SELECT * FROM d;
SELECT * FROM gci_t ORDER BY a;
-- Aggregates over sub-selects in an inlined CTE.
EXPLAIN (VERBOSE, COSTS OFF)
WITH a AS (SELECT id FROM (VALUES (1), (2)) AS v(id)), b AS (SELECT max((SELECT sum(id) FROM a)) AS agg)
SELECT agg FROM b;
WITH a AS (SELECT id FROM (VALUES (1), (2)) AS v(id)), b AS (SELECT max((SELECT sum(id) FROM a)) AS agg)
SELECT agg FROM b;
