-- GiST indexes of btree_gist integer columns, alone and with a second column (agent 187):
-- their sizes from the sorted build (by the first column, then the second; gistSplitByKey's
-- don't-care tuples split again by the second column), after later inserts and VACUUM, and the
-- KNN choice between an index on (p) and one on (cat, p) that the sizes decide.
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE a187_gt (id int, cat int, big int8, small int2, p point, r int4range);
INSERT INTO a187_gt SELECT i, i % 7, i::int8 * 1000003 % 5003, (i % 300)::int2,
  point((i * 7919) % 2003 / 2.0, (i * 104729) % 1999 / 2.0),
  int4range(i % 500, i % 500 + 1 + i % 9)
  FROM generate_series(1, 6000) i;
INSERT INTO a187_gt VALUES (6001, 1, NULL, NULL, NULL, NULL), (6002, NULL, 5, 5, point(500, 500), 'empty');
CREATE INDEX a187_gt_p ON a187_gt USING gist (p);
CREATE INDEX a187_gt_cat ON a187_gt USING gist (cat);
CREATE INDEX a187_gt_big ON a187_gt USING gist (big);
CREATE INDEX a187_gt_small ON a187_gt USING gist (small);
CREATE INDEX a187_gt_cp ON a187_gt USING gist (cat, p);
CREATE INDEX a187_gt_pc ON a187_gt USING gist (p, cat);
CREATE INDEX a187_gt_cr ON a187_gt USING gist (cat, r);
CREATE INDEX a187_gt_bs ON a187_gt USING gist (big, small);
ANALYZE a187_gt;
SELECT c.relname, pg_relation_size(c.oid) / 8192 AS pages, c.relpages
  FROM pg_class c WHERE c.relname LIKE 'a187_gt%' ORDER BY 1;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM a187_gt ORDER BY p <-> point(250, 250) LIMIT 5;
EXPLAIN (COSTS OFF) SELECT id FROM a187_gt WHERE cat = 3 ORDER BY p <-> point(250, 250) LIMIT 5;
SELECT id FROM a187_gt WHERE cat = 3 ORDER BY p <-> point(250, 250) LIMIT 5;
RESET enable_seqscan;
RESET enable_bitmapscan;
-- Rows after the build go in by gistchoose and split pages as gistSplit does.
INSERT INTO a187_gt SELECT 7000 + i, i % 3, i, (i % 7)::int2, point(250 + i / 100.0, 250 - i / 100.0), int4range(i, i + 2)
  FROM generate_series(1, 3000) i;
SELECT c.relname, pg_relation_size(c.oid) / 8192 AS pages
  FROM pg_class c WHERE c.relname LIKE 'a187_gt_%' ORDER BY 1;
DELETE FROM a187_gt WHERE id % 4 = 0;
VACUUM a187_gt;
SELECT c.relname, pg_relation_size(c.oid) / 8192 AS pages, c.relpages
  FROM pg_class c WHERE c.relname LIKE 'a187_gt%' ORDER BY 1;
-- An exclusion constraint's index: (room, during).
CREATE TABLE a187_gx (room int, during int4range, EXCLUDE USING gist (room WITH =, during WITH &&));
INSERT INTO a187_gx SELECT i % 40, int4range(i / 40 * 10, i / 40 * 10 + 5) FROM generate_series(1, 4000) i;
SELECT pg_relation_size('a187_gx_room_during_excl') / 8192 AS pages;
DROP TABLE a187_gx;
DROP TABLE a187_gt;
