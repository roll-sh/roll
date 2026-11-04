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

EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT o.k, (SELECT count(*) FROM a167_r r JOIN a167_n n ON n.rk = r.rk WHERE r.rk < o.k % 3)
FROM a167_o o WHERE o.k <= 6;
-- Two and one outer rows: total three, which cannot be reconstructed by rounding an average.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT o.k, (SELECT count(*) FROM a167_r r JOIN a167_n n ON n.rk = r.rk WHERE r.rk < 2 + o.k % 2)
FROM a167_o o WHERE o.k <= 2;

RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;
RESET enable_memoize;
DROP TABLE a167_o, a167_r, a167_n;
