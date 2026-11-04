-- Cached nested-loop inputs across SubPlan runs with varying outer row counts.
CREATE TABLE a167_o (k int);
CREATE TABLE a167_r (rk int);
CREATE TABLE a167_n (nk int, rk int);
INSERT INTO a167_o SELECT g FROM generate_series(1, 6) g;
INSERT INTO a167_r VALUES (1), (2), (3);
INSERT INTO a167_n SELECT g, g % 3 + 1 FROM generate_series(1, 9) g;
ANALYZE a167_o, a167_r, a167_n;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
SET enable_memoize = off;

-- Once-only InitPlan first evaluated by a cached inner scan.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM a167_o o LEFT JOIN a167_n n ON n.rk=o.k AND n.nk < (SELECT max(rk)*3 FROM a167_r);
-- A genuinely correlated SubPlan in the inner filter must retain per-rescan counts.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM a167_o o LEFT JOIN a167_n n ON n.rk=o.k
 AND n.nk < (SELECT max(r.rk+n.rk) FROM a167_r r);
-- The CTE producer is owned by the outer query level, while its CTE Scan is rescanned.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
WITH c AS MATERIALIZED (SELECT * FROM a167_n)
SELECT * FROM a167_o o LEFT JOIN c ON c.rk=o.k;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;
RESET enable_memoize;
DROP TABLE a167_o, a167_r, a167_n;
