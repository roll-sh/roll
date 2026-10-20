-- DISTINCT and DISTINCT ON in the order an index reads: DISTINCT ON's keys reordered to
-- an index's (the ORDER BY then sorting the distinct rows), DISTINCT's sort following
-- the ORDER BY's direction, constant keys making one group.
CREATE TABLE gpl_d (x int, y int, z int);
INSERT INTO gpl_d SELECT i % 10, i % 7, i FROM generate_series(1, 1000) i;
CREATE INDEX gpl_d_xy ON gpl_d (x, y);
VACUUM ANALYZE gpl_d;
SET enable_hashagg = off;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT DISTINCT ON (y, x) x, y FROM gpl_d;
SELECT DISTINCT ON (y, x) x, y FROM gpl_d ORDER BY y, x LIMIT 5;
EXPLAIN (COSTS OFF) SELECT DISTINCT ON (y, x) x, y FROM gpl_d ORDER BY y;
SELECT DISTINCT ON (y, x) x, y FROM gpl_d ORDER BY y, x LIMIT 3;
RESET enable_hashagg;
EXPLAIN (COSTS OFF) SELECT DISTINCT x FROM gpl_d WHERE x IN (2, 3, 5) ORDER BY x DESC;
SELECT DISTINCT x FROM gpl_d WHERE x IN (2, 3, 5) ORDER BY x DESC;
SELECT DISTINCT x FROM gpl_d WHERE x > 6 ORDER BY x DESC;
RESET enable_seqscan;
EXPLAIN (COSTS OFF) SELECT DISTINCT ON (x) x, z FROM gpl_d WHERE x = 3 ORDER BY 1, 2;
SELECT DISTINCT ON (x) x, z FROM gpl_d WHERE x = 3 ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT x, y, count(*) FROM gpl_d WHERE x = 1 GROUP BY x, y;
EXPLAIN (COSTS OFF) SELECT y, count(*) FROM gpl_d WHERE x = 1 GROUP BY x, y ORDER BY y;
SELECT y, count(*) FROM gpl_d WHERE x = 1 GROUP BY x, y ORDER BY y;
