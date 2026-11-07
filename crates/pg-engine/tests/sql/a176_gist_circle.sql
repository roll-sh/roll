-- GiST point-in-circle and circle-overlap scans (agent 176): the search structure skips
-- entries whose cell lies outside the circle; the results must be the full scan's, at the
-- circle's edge too.
CREATE TABLE gc (id int, p point);
INSERT INTO gc SELECT g, point((g * 37) % 100 + (g % 7) / 10.0, (g * 91) % 100 + (g % 3) / 4.0)
  FROM generate_series(1, 20000) g;
-- On or about the edge of circle((50,50), 20).
INSERT INTO gc VALUES (100001, point(70, 50)), (100002, point(50, 30)), (100003, point(70.0000005, 50)),
  (100004, point(50, 69.9999999)), (100005, point(70.00001, 50)), (100006, point(64.142135623730950, 64.142135623730950)),
  (100007, point(64.1421357, 64.1421357)),
  (100010, NULL);
CREATE INDEX gc_p ON gc USING gist (p);
ANALYZE gc;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT id FROM gc WHERE p <@ circle(point(50, 50), 20) AND id > 100000 ORDER BY id;
SELECT count(*) FROM gc WHERE p <@ circle(point(0, 0), 15);
SELECT count(*) FROM gc WHERE p <@ circle(point(99.5, 0.5), 3);
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 0);
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 1e9);
SELECT count(*) FROM gc WHERE circle(point(25, 75), 10) @> p;
SELECT id FROM gc WHERE p <@ circle(point(25, 75), 10) LIMIT 3;
RESET enable_bitmapscan;
SET enable_indexscan = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
RESET enable_indexscan;
RESET enable_seqscan;
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT count(*) FROM gc WHERE p <@ circle(point(0, 0), 15);
-- The transaction's own rows.
BEGIN;
INSERT INTO gc SELECT 200000 + g, point(50 + 19.99 * cos(g), 50 + 19.99 * sin(g)) FROM generate_series(1, 2000) g;
INSERT INTO gc VALUES (300001, point(70.001, 50));
SET LOCAL enable_seqscan = off;
SET LOCAL enable_bitmapscan = off;
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20) AND id > 300000;
SET LOCAL enable_bitmapscan = on;
SET LOCAL enable_indexscan = off;
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20) AND id > 300000;
SELECT count(*) FROM gc WHERE p <@ box(point(30, 30), point(70, 70));
ROLLBACK;
-- Rows written again since the index's structure was built: moved in, moved out.
UPDATE gc SET p = point(50, 50) WHERE id IN (100005, 100007);
UPDATE gc SET p = point(0, 0) WHERE id IN (100001, 100002);
SET enable_seqscan = off;
SET enable_indexscan = off;
SELECT id FROM gc WHERE p <@ circle(point(50, 50), 20) AND id > 100000 ORDER BY id;
SELECT count(*) FROM gc WHERE p <@ circle(point(50, 50), 20);
SELECT count(*) FROM gc WHERE p <@ box(point(30, 30), point(70, 70));
SELECT count(*) FROM gc WHERE p <@ polygon '((40,40),(60,40),(50,70))';
SELECT count(*) FROM gc WHERE p << point(10, 0) AND p <@ box(point(0, 0), point(20, 20));
SELECT id FROM gc WHERE p ~= point(50, 50) ORDER BY id;
RESET enable_indexscan;
RESET enable_seqscan;
-- Circles overlapping a circle.
CREATE TABLE gcc (id int, c circle);
INSERT INTO gcc SELECT g, circle(point((g * 37) % 100, (g * 91) % 100), (g % 5) + 0.5) FROM generate_series(1, 20000) g;
INSERT INTO gcc VALUES (100001, circle(point(80, 50), 10)), (100002, circle(point(80.0000001, 50), 10)), (100003, circle(point(80.001, 50), 10));
CREATE INDEX gcc_c ON gcc USING gist (c);
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT count(*) FROM gcc WHERE c && circle(point(50, 50), 20);
SELECT id FROM gcc WHERE c && circle(point(50, 50), 20) AND id > 100000 ORDER BY id;
SELECT count(*) FROM gcc WHERE c <@ circle(point(50, 50), 20);
RESET enable_seqscan;
RESET enable_bitmapscan;
SELECT count(*) FROM gcc WHERE c && circle(point(50, 50), 20);
SELECT count(*) FROM gcc WHERE c <@ circle(point(50, 50), 20);
DROP TABLE gc, gcc;
