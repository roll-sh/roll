-- Row comparisons kept whole (RowCompareExpr): evaluated pair by pair, shown as
-- ROW(...) op ROW(...), and btree index conditions (cut to the index's columns).
CREATE TABLE gpl_rc (a int, b int, c text);
INSERT INTO gpl_rc VALUES (1, 1, 'a'), (1, 2, 'b'), (2, 1, 'c'), (2, NULL, 'd'), (NULL, 1, 'e'), (3, 3, 'f');
SELECT * FROM gpl_rc WHERE (a, b) > (1, 1) ORDER BY 1, 2, 3;
SELECT * FROM gpl_rc WHERE (a, b) >= (1, 2) ORDER BY 1, 2, 3;
SELECT * FROM gpl_rc WHERE (a, b) < (2, 1) ORDER BY 1, 2, 3;
SELECT * FROM gpl_rc WHERE (a, b) <= (2, NULL) ORDER BY 1, 2, 3;
SELECT (1, 2) < (1, 3), (1, NULL) < (2, 1), (1, NULL) < (1, 2), (2, NULL) > (1, 5), (NULL, 1) > (1, 1);
SELECT (a, c) > (1, 'a') FROM gpl_rc ORDER BY a, c;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_rc WHERE (a, b) > (1, 1);
EXPLAIN (VERBOSE, COSTS OFF) SELECT (a, b) <= (2, 3), ROW(a, c) < ROW(1, 'x'::text) FROM gpl_rc;
EXPLAIN (COSTS OFF) SELECT * FROM gpl_rc WHERE (a, 1) < (2, 1);
CREATE VIEW gpl_rcv AS SELECT * FROM gpl_rc WHERE (a, b) > (1, 2);
SELECT pg_get_viewdef('gpl_rcv');
SELECT * FROM gpl_rcv ORDER BY 1, 2;
CREATE TABLE gpl_rt2 (x int, y int, z int);
INSERT INTO gpl_rt2 SELECT i / 100, i % 100, i % 7 FROM generate_series(1, 10000) i;
CREATE INDEX gpl_rt2_xy ON gpl_rt2 (x, y);
ANALYZE gpl_rt2;
EXPLAIN (COSTS OFF) SELECT x, y FROM gpl_rt2 WHERE (x, y) >= (97, 50) ORDER BY x, y;
SELECT x, y FROM gpl_rt2 WHERE (x, y) >= (99, 97) ORDER BY x, y;
EXPLAIN (COSTS OFF) SELECT x, y, z FROM gpl_rt2 WHERE (x, y, z) > (98, 50, 3) ORDER BY x, y;
SELECT count(*) FROM gpl_rt2 WHERE (x, y, z) > (98, 50, 3);
EXPLAIN (COSTS OFF) SELECT x, y FROM gpl_rt2 WHERE (98, 50) < (x, y) ORDER BY x, y;
SELECT count(*) FROM gpl_rt2 WHERE (98, 50) < (x, y);
