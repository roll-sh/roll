-- CACHE-ONLY DRAFT: unexecuted on PG and engine. No .out/golden is asserted.
-- Existing a167 fixtures, one new index, four tiny EXPLAIN cases.
CREATE TABLE a167_o(k int);
CREATE TABLE a167_r(rk int);
CREATE TABLE a167_n(nk int, rk int);
INSERT INTO a167_o SELECT g FROM generate_series(1,6) g;
INSERT INTO a167_r VALUES (1),(2),(3);
INSERT INTO a167_n SELECT g, g%3+1 FROM generate_series(1,9) g;
CREATE INDEX a167_r_rk_idx ON a167_r(rk);
ANALYZE a167_o,a167_r,a167_n;
SET enable_hashjoin=off;
SET enable_mergejoin=off;
SET enable_material=off;
SET enable_memoize=off;
SET max_parallel_workers_per_gather=0;

-- A: named N1. Force consideration of MIN/MAX over the ordered index.
-- Required shape: cached n scan -> correlated SubPlan -> nested InitPlan/Limit/index.
-- Hypothesis only: PG SubPlan and nested Limit each loops54; d9e root54/nested9.
SET enable_seqscan=off;
EXPLAIN (ANALYZE,COSTS OFF,TIMING OFF,SUMMARY OFF,BUFFERS OFF)
SELECT * FROM a167_o o LEFT JOIN a167_n n ON n.rk=o.k
 AND n.nk < (SELECT max(r.rk) FROM a167_r r WHERE r.rk<=n.rk);
SELECT count(*) FROM a167_o o LEFT JOIN a167_n n ON n.rk=o.k
 AND n.nk < (SELECT max(r.rk) FROM a167_r r WHERE r.rk<=n.rk);
RESET enable_seqscan;

-- B: statement-cache control *inside* a freshly framed correlated SubPlan.
-- The independent nested SUM must remain once, despite the correlated root rescans.
-- MAX(r.rk+n.rk) avoids the indexed MIN/MAX rewrite for the correlated root.
EXPLAIN (ANALYZE,COSTS OFF,TIMING OFF,SUMMARY OFF,BUFFERS OFF)
SELECT * FROM a167_o o LEFT JOIN a167_n n ON n.rk=o.k
 AND n.nk < (SELECT max(r.rk+n.rk)
               + (SELECT sum(z.rk)::int FROM a167_r z) FROM a167_r r);

-- C: an empty-parameter nested query that reads a CTE bound to a fresh n frame.
-- Required shape: correlated SubPlan, dependent CTE producer, nested MAX InitPlan.
-- Judge the nested aggregate separately from the producer; this does not cover
-- N2's pre-existing nonparameterized local-WITH tuplestore rewind discrepancy.
EXPLAIN (ANALYZE,COSTS OFF,TIMING OFF,SUMMARY OFF,BUFFERS OFF)
SELECT * FROM a167_o o LEFT JOIN a167_n n ON n.rk=o.k
 AND n.nk < (WITH c AS MATERIALIZED
                 (SELECT r.rk+n.rk AS v FROM a167_r r)
             SELECT (SELECT max(v) FROM c));

-- D: cache tied to a frame that already exists at this collect boundary.
-- It must stay excluded from the inner replay, even though enclosing is true.
-- Index paths are disabled only for this guard to retain the small r -> n shape.
SET enable_indexscan=off;
SET enable_indexonlyscan=off;
SET enable_bitmapscan=off;
EXPLAIN (ANALYZE,COSTS OFF,TIMING OFF,SUMMARY OFF,BUFFERS OFF)
SELECT o.k, (SELECT count(*) FROM a167_r r JOIN a167_n n ON n.rk=r.rk
 WHERE r.rk < 2+o.k%2
 AND n.nk <= (SELECT max(z.rk) FROM a167_r z WHERE z.rk<=o.k))
FROM a167_o o WHERE o.k<=2;
RESET enable_indexscan;
RESET enable_indexonlyscan;
RESET enable_bitmapscan;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;
RESET enable_memoize;
RESET max_parallel_workers_per_gather;
DROP TABLE a167_o,a167_r,a167_n;
