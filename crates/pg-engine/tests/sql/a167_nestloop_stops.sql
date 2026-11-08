-- A nested loop stops where ExecNestLoop stops: a semi join at its first match for each outer
-- row, and an aggregation by the outer side's order under a LIMIT at the group past the
-- limit, so a row or group PostgreSQL never reaches can't raise an error (difftest round 10).
CREATE TABLE a167_lo (id int PRIMARY KEY, k int);
INSERT INTO a167_lo SELECT i, i % 40 FROM generate_series(1, 1000) i;
CREATE TABLE a167_li (id int PRIMARY KEY, k int, v int);
INSERT INTO a167_li SELECT i, i % 50, i % 7 FROM generate_series(1, 5000) i;
CREATE INDEX a167_li_k ON a167_li (k);
ANALYZE a167_lo, a167_li;
SET enable_hashjoin = off;
SET enable_mergejoin = off;

-- semi and anti joins over an inner index scan or a bitmap heap scan
SELECT count(*) FROM a167_lo o WHERE EXISTS (SELECT 1 FROM a167_li i WHERE i.k = o.k AND 1 / (i.id - 1601) > -5) AND o.id < 3;
SET enable_indexscan = off;
SELECT count(*) FROM a167_lo o WHERE EXISTS (SELECT 1 FROM a167_li i WHERE i.k = o.k AND 1 / (i.id - 1601) > -5) AND o.id < 3;
SELECT count(*) FROM a167_lo o WHERE NOT EXISTS (SELECT 1 FROM a167_li i WHERE i.k = o.k AND 1 / (i.id - 1601) > -5) AND o.id < 3;
RESET enable_indexscan;

-- a GroupAggregate under a LIMIT: groups past it aren't computed
EXPLAIN (COSTS OFF)
SELECT o.id, count(i.id), sum(i.v / (o.id - 30)) FROM a167_lo o JOIN a167_li i ON i.k = o.k WHERE o.id < 40 GROUP BY o.id ORDER BY o.id LIMIT 5;
SELECT o.id, count(i.id), sum(i.v / (o.id - 30)) FROM a167_lo o JOIN a167_li i ON i.k = o.k WHERE o.id < 40 GROUP BY o.id ORDER BY o.id LIMIT 5;
-- a left join, and the error reached when the limit needs that group
SELECT o.id, count(i.id), sum(i.v / (o.id - 8)) FROM a167_lo o LEFT JOIN a167_li i ON i.k = o.k + 45 WHERE o.id < 40 GROUP BY o.id ORDER BY o.id LIMIT 6;
SELECT o.id, count(i.id), sum(i.v / (o.id - 4)) FROM a167_lo o JOIN a167_li i ON i.k = o.k WHERE o.id < 40 GROUP BY o.id ORDER BY o.id LIMIT 5;
-- one table's GroupAggregate under a LIMIT (no Sort in the plan: the groups come in order)
SET enable_hashagg = off;
EXPLAIN (COSTS OFF)
SELECT id, sum(1 / (id - 30)) FROM a167_lo WHERE id < 40 GROUP BY id ORDER BY id LIMIT 3;
SELECT id, sum(1 / (id - 30)) FROM a167_lo WHERE id < 40 GROUP BY id ORDER BY id LIMIT 3;
RESET enable_hashagg;

RESET enable_hashjoin;
RESET enable_mergejoin;
DROP TABLE a167_lo, a167_li;
