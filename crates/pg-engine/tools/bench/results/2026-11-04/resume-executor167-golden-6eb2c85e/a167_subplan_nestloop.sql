-- A nested loop whose inner side isn't parameterized (Postgres rescans it per outer row; the
-- executor reads it once per run of the join) inside a SubPlan run many times: EXPLAIN
-- ANALYZE's loops and rows are per outer row of each run, not of all runs squared (TPC-H
-- q02's region / nation join).
CREATE TABLE a167_o (k int);
CREATE TABLE a167_r (rk int, name text);
CREATE TABLE a167_n (nk int, rk int);
CREATE TABLE a167_s (sk int, nk int, v int);
INSERT INTO a167_o SELECT g FROM generate_series(1, 30) g;
INSERT INTO a167_r VALUES (1, 'EUROPE'), (2, 'ASIA'), (3, 'AFRICA');
INSERT INTO a167_n SELECT g, g % 3 + 1 FROM generate_series(1, 9) g;
INSERT INTO a167_s SELECT g, g % 9 + 1, g * 7 % 100 FROM generate_series(1, 300) g;
CREATE INDEX a167_n_nk ON a167_n (nk);
ANALYZE a167_o, a167_r, a167_n, a167_s;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
SET enable_memoize = off;

EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT o.k, (SELECT min(s.v) FROM a167_r r JOIN (a167_s s JOIN a167_n n ON n.nk = s.nk) ON r.rk = n.rk
             WHERE r.name = 'EUROPE' AND s.sk = o.k * 9 + 2) AS m
FROM a167_o o;
SELECT o.k, (SELECT min(s.v) FROM a167_r r JOIN (a167_s s JOIN a167_n n ON n.nk = s.nk) ON r.rk = n.rk
             WHERE r.name = 'EUROPE' AND s.sk = o.k * 9 + 2) AS m
FROM a167_o o ORDER BY o.k;
-- two outer rows per run of the join
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT o.k, (SELECT count(*) FROM a167_r r JOIN a167_n n ON n.rk = r.rk + o.k % 2 WHERE r.rk < 3) AS c
FROM a167_o o WHERE o.k <= 10;

RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;
RESET enable_memoize;
DROP TABLE a167_o, a167_r, a167_n, a167_s;
