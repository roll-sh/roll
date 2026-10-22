-- A relation proven empty keeps the equivalence classes its quals made: a GROUP BY or ORDER
-- BY key equated with a constant is dropped (a GroupAggregate without keys); its columns are
-- qualified only when the plan scans no relation at all (set_rtable_names names every
-- relation then), else shown bare. Group keys equated with constants don't count in the
-- grouping's cost; a grouping's keys keep their classes for an ORDER BY above. A
-- partitioned table's whole row above its partitions is a converted row.
CREATE TABLE a6_pt (a int, b int, c text) PARTITION BY LIST (c);
CREATE TABLE a6_pt1 PARTITION OF a6_pt FOR VALUES IN ('0000', '0001', '0002');
CREATE TABLE a6_pt2 PARTITION OF a6_pt FOR VALUES IN ('0003', '0004');
INSERT INTO a6_pt SELECT i % 20, i % 30, to_char(i % 5, 'FM0000') FROM generate_series(0, 999) i;
ANALYZE a6_pt;
CREATE TABLE a6_r1 (x int, y int) PARTITION BY RANGE (x);
CREATE TABLE a6_r1_p1 PARTITION OF a6_r1 FOR VALUES FROM (0) TO (10);
CREATE TABLE a6_r1_p2 PARTITION OF a6_r1 FOR VALUES FROM (10) TO (30);
CREATE TABLE a6_r2 (x int, y int) PARTITION BY RANGE (y);
CREATE TABLE a6_r2_p1 PARTITION OF a6_r2 FOR VALUES FROM (0) TO (10);
CREATE TABLE a6_r2_p2 PARTITION OF a6_r2 FOR VALUES FROM (10) TO (30);
INSERT INTO a6_r1 SELECT i % 30, i % 20 FROM generate_series(0, 299, 2) i;
INSERT INTO a6_r2 SELECT i % 20, i % 30 FROM generate_series(0, 299, 3) i;
ANALYZE a6_r1;
ANALYZE a6_r2;
EXPLAIN (COSTS OFF) SELECT c, sum(a) FROM a6_pt WHERE c = 'x' GROUP BY c;
SELECT c, sum(a) FROM a6_pt WHERE c = 'x' GROUP BY c;
EXPLAIN (COSTS OFF) SELECT c, sum(a) FROM a6_pt WHERE 1 = 2 GROUP BY c;
EXPLAIN (COSTS OFF)
SELECT a.x, a.y, count(*) FROM (SELECT * FROM a6_r1 WHERE x = 1 AND x = 2) a
  LEFT JOIN a6_r2 b ON a.x = b.y GROUP BY a.x, a.y ORDER BY 1, 2;
SELECT a.x, a.y, count(*) FROM (SELECT * FROM a6_r1 WHERE x = 1 AND x = 2) a
  LEFT JOIN a6_r2 b ON a.x = b.y GROUP BY a.x, a.y ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT a.x, a.y, count(*) FROM (SELECT * FROM a6_r1_p1 WHERE x = 3 AND x = 4) a GROUP BY a.x, a.y ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT x, y, count(*) FROM a6_r1_p1 WHERE x = 3 AND x = 4 GROUP BY x, y ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT x, y, count(*) FROM a6_r1 WHERE x = 300 GROUP BY x, y ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT x, count(*) FROM a6_r1 WHERE x = 300 GROUP BY x ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT x, y, count(*) FROM a6_r1 WHERE x = 300 AND y = 3 GROUP BY x, y ORDER BY 1, 2;
EXPLAIN
SELECT x, y, count(*) FROM a6_r1 WHERE x = 3 AND y = 3 GROUP BY x, y ORDER BY 1, 2;
SELECT x, y, count(*) FROM a6_r1 WHERE x = 3 AND y = 3 GROUP BY x, y ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT a.x, a.y, count(*) FROM (SELECT * FROM a6_r1_p1 WHERE x = 3) a GROUP BY a.x, a.y ORDER BY 1, 2;
SELECT a, count(*) FROM a6_pt WHERE c = '0001' GROUP BY c, a ORDER BY 1;
-- A dummy relation beside scanned ones: its columns bare.
EXPLAIN (COSTS OFF)
SELECT t1.x, t2.y FROM (SELECT * FROM a6_r1 WHERE x = 1 AND x = 2) t1
  RIGHT JOIN a6_r2 t2 ON t1.x = t2.y AND t1.y = t2.x;
-- Whole rows of a partitioned table.
EXPLAIN (COSTS OFF)
SELECT t1.x, sum(t1.y), count(t1) FROM a6_r1 t1, a6_r2 t2 WHERE t1.x = t2.y GROUP BY t1.x ORDER BY 1, 2, 3;
SELECT t1.x, sum(t1.y), count(t1) FROM a6_r1 t1, a6_r2 t2 WHERE t1.x = t2.y GROUP BY t1.x ORDER BY 1, 2, 3;
DROP TABLE a6_pt, a6_r1, a6_r2;
