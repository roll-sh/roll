-- An ORDER BY over a partitioned table read partition by partition in bound order (a
-- plain Append, backwards for DESC) when the partitions can't interleave: RANGE without a
-- live default, LIST without interleaved values; otherwise a Merge Append, nested for an
-- unordered sub-partitioned table.
CREATE TABLE gpl_oa (a int, b int, c int) PARTITION BY RANGE (a, b);
CREATE TABLE gpl_oa1 PARTITION OF gpl_oa FOR VALUES FROM (0, 0) TO (10, 10);
CREATE TABLE gpl_oa2 PARTITION OF gpl_oa FOR VALUES FROM (10, 10) TO (20, 20);
CREATE TABLE gpl_oa3 PARTITION OF gpl_oa FOR VALUES FROM (20, 20) TO (30, 30) PARTITION BY LIST (a);
CREATE TABLE gpl_oa3a PARTITION OF gpl_oa3 FOR VALUES IN (20, 22);
CREATE TABLE gpl_oa3b PARTITION OF gpl_oa3 FOR VALUES IN (21);
CREATE INDEX ON gpl_oa (a, b, c);
INSERT INTO gpl_oa SELECT i % 23, i % 23, i FROM generate_series(1, 690) i;
VACUUM ANALYZE gpl_oa;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_oa WHERE a < 20 ORDER BY a, b, c;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_oa WHERE a < 20 ORDER BY a DESC, b DESC, c DESC;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_oa ORDER BY a, b;
SELECT a, b, c FROM gpl_oa ORDER BY a, b, c LIMIT 3;
SELECT a, b, c FROM gpl_oa ORDER BY a DESC, b DESC, c DESC LIMIT 3;
SELECT a, count(*) FROM (SELECT a FROM gpl_oa WHERE a BETWEEN 18 AND 23 ORDER BY a, b) s GROUP BY a ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_oa WHERE a IN (21, 22) ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_oa WHERE a = 5 ORDER BY a, b, c;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE gpl_oa;
