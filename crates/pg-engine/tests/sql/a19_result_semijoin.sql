-- remove_useless_result_rtes for a semi join: an IN whose sub-select, pulled up, is a
-- FROM-less relation (or a join on true of such relations, after reduce_outer_joins)
-- returns each left row or not, so the semi join is its left side under the join's
-- clause, the right side's values evaluated there (a FROM-less left side's Result gates
-- on it: a One-Time Filter).
EXPLAIN (VERBOSE, COSTS OFF)
WITH ctetable AS NOT MATERIALIZED (SELECT 1 AS f1)
SELECT * FROM ctetable c1
WHERE f1 IN (SELECT c3.f1 FROM ctetable c2 FULL JOIN ctetable c3 ON true);
WITH ctetable AS NOT MATERIALIZED (SELECT 1 AS f1)
SELECT * FROM ctetable c1
WHERE f1 IN (SELECT c3.f1 FROM ctetable c2 FULL JOIN ctetable c3 ON true);
WITH ctetable AS NOT MATERIALIZED (SELECT 1 AS f1)
SELECT * FROM ctetable c1
WHERE f1 IN (SELECT c3.f1 + 1 FROM ctetable c2 FULL JOIN ctetable c3 ON true);
CREATE TABLE a19_rs (a int, b int);
INSERT INTO a19_rs SELECT i, i % 7 FROM generate_series(1, 20) i;
ANALYZE a19_rs;
EXPLAIN (COSTS OFF)
SELECT * FROM a19_rs WHERE a IN (SELECT x FROM (SELECT 5 AS x) s1 LEFT JOIN (SELECT 6 AS y) s2 ON true);
SELECT * FROM a19_rs WHERE a IN (SELECT x FROM (SELECT 5 AS x) s1 LEFT JOIN (SELECT 6 AS y) s2 ON true);
EXPLAIN (COSTS OFF)
SELECT * FROM a19_rs WHERE a IN (SELECT s2.y FROM (SELECT 5 AS x) s1 FULL JOIN (SELECT 6 AS y) s2 ON true);
SELECT * FROM a19_rs WHERE a IN (SELECT s2.y FROM (SELECT 5 AS x) s1 FULL JOIN (SELECT 6 AS y) s2 ON true);
