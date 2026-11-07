-- GiST KNN over two-dimensional grids walked strip by strip (agent 163): points, boxes,
-- circles and polygons at origins inside, at the edge of and far outside the data, with a
-- filter, an equality column of btree_gist (rows by partition), rows written after the
-- structure was built, and the transaction's own rows.
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE a163_ks (id int, cat int, p point, b box, c circle, g polygon);
INSERT INTO a163_ks SELECT i, i % 7,
  point((i * 7919) % 2003 / 2.0, (i * 104729) % 1999 / 2.0),
  box(point((i * 7919) % 2003 / 2.0, (i * 104729) % 1999 / 2.0), point((i * 7919) % 2003 / 2.0 + i % 5, (i * 104729) % 1999 / 2.0 + i % 3)),
  circle(point((i * 6007) % 2011 / 2.0, (i * 3001) % 1993 / 2.0), 0.5 + i % 4),
  polygon(box(point((i * 4001) % 2017 / 2.0, (i * 2003) % 1987 / 2.0), point((i * 4001) % 2017 / 2.0 + 1.5, (i * 2003) % 1987 / 2.0 + 0.75)))
  FROM generate_series(1, 6000) i;
INSERT INTO a163_ks VALUES (6001, 1, NULL, NULL, NULL, NULL), (6002, 2, point(500, 500), box(point(500, 500), point(500, 500)), circle(point(500, 500), 0), NULL);
CREATE INDEX a163_ks_p ON a163_ks USING gist (p);
CREATE INDEX a163_ks_b ON a163_ks USING gist (b);
CREATE INDEX a163_ks_c ON a163_ks USING gist (c);
CREATE INDEX a163_ks_g ON a163_ks USING gist (g);
ANALYZE a163_ks;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM a163_ks ORDER BY p <-> point(250, 250) LIMIT 5;
SELECT id, round((p <-> point(250, 250))::numeric, 6) FROM a163_ks ORDER BY p <-> point(250, 250) LIMIT 8;
SELECT id FROM a163_ks ORDER BY p <-> point(0, 0) LIMIT 6;
SELECT id FROM a163_ks ORDER BY p <-> point(1001.3, 999.7) LIMIT 6;
SELECT id FROM a163_ks ORDER BY p <-> point(-5000, 300) LIMIT 4;
SELECT id FROM a163_ks ORDER BY p <-> point(500, 1e6) LIMIT 4;
SELECT id FROM a163_ks WHERE id % 5 = 0 ORDER BY p <-> point(731, 222) LIMIT 7;
SELECT count(*) FROM (SELECT id FROM a163_ks ORDER BY p <-> point(400, 400) LIMIT 3000) s;
SELECT id, round((b <-> point(321.5, 654.25))::numeric, 6) FROM a163_ks ORDER BY b <-> point(321.5, 654.25) LIMIT 8;
SELECT id, round((c <-> point(10, 990))::numeric, 6) FROM a163_ks ORDER BY c <-> point(10, 990) LIMIT 8;
SELECT id, round((g <-> point(612, 88))::numeric, 6) FROM a163_ks ORDER BY g <-> point(612, 88) LIMIT 8;
-- (Made here: with it, the engine plans the queries above on it, PostgreSQL on a163_ks_p.)
CREATE INDEX a163_ks_cp ON a163_ks USING gist (cat, p);
EXPLAIN (COSTS OFF) SELECT id FROM a163_ks WHERE cat = 3 ORDER BY p <-> point(250, 250) LIMIT 5;
SELECT id FROM a163_ks WHERE cat = 3 ORDER BY p <-> point(250, 250) LIMIT 6;
SELECT id FROM a163_ks WHERE cat = 6 ORDER BY p <-> point(999, 0) LIMIT 6;
-- Rows after the build, then the transaction's own.
INSERT INTO a163_ks SELECT 7000 + i, i % 7, point(250 + i / 1000.0, 250 - i / 1000.0), NULL, NULL, NULL FROM generate_series(1, 50) i;
SELECT id FROM a163_ks ORDER BY p <-> point(250, 250) LIMIT 6;
BEGIN;
INSERT INTO a163_ks VALUES (8000, 3, point(250.0001, 250.0001), NULL, NULL, NULL);
DELETE FROM a163_ks WHERE id = 7001;
SELECT id FROM a163_ks ORDER BY p <-> point(250, 250) LIMIT 4;
SELECT id FROM a163_ks WHERE cat = 3 ORDER BY p <-> point(250, 250) LIMIT 3;
ROLLBACK;
DELETE FROM a163_ks WHERE id BETWEEN 7001 AND 7040;
SELECT id FROM a163_ks ORDER BY p <-> point(250, 250) LIMIT 6;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a163_ks;
