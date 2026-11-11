-- A GiST point_ops index after mixed DML (agent 127): its model replays the index's
-- history (the build, each entry inserted, each VACUUM's removals), so an unordered scan
-- returns rows in PostgreSQL's order and the index has its page count, after updates that
-- move points, deletes, rolled back inserts and VACUUM.
CREATE TABLE a127_pt (id int, p point) WITH (autovacuum_enabled = off);
INSERT INTO a127_pt SELECT g, point((g * 37) % 100, (g * 91) % 100) FROM generate_series(1, 600) g;
CREATE INDEX a127_pt_p ON a127_pt USING gist (p);
ANALYZE a127_pt;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
INSERT INTO a127_pt SELECT g, point((g * 13) % 100, (g * 7) % 100) FROM generate_series(601, 700) g;
UPDATE a127_pt SET p = point(20, 20) WHERE id % 50 = 0;
DELETE FROM a127_pt WHERE id % 7 = 0;
EXPLAIN (COSTS OFF) SELECT id FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
-- (Not the moved rows' KNN order before the VACUUM: their old entries, dead, are marked
-- LP_DEAD by the scan above, which changes the order of equally distant rows; not modelled.)
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a127_pt ORDER BY p <-> point '(73.3,71.1)' LIMIT 30) s;
SELECT pg_relation_size('a127_pt_p') / 8192 AS pages;
BEGIN;
INSERT INTO a127_pt SELECT g, point(g % 100, (g * 3) % 100) FROM generate_series(1001, 1400) g;
ROLLBACK;
SELECT string_agg(id::text, ',') FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
SELECT pg_relation_size('a127_pt_p') / 8192 AS pages;
VACUUM a127_pt;
SELECT relpages FROM pg_class WHERE relname = 'a127_pt_p';
SELECT pg_relation_size('a127_pt_p') / 8192 AS pages;
SELECT string_agg(id::text, ',') FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a127_pt ORDER BY p <-> point '(20,20)' LIMIT 30) s;
-- Inserts after the VACUUM.
INSERT INTO a127_pt SELECT g, point((g * 17) % 100, (g * 29) % 100) FROM generate_series(2001, 2600) g;
UPDATE a127_pt SET p = point(p[0] + 0.5, p[1]) WHERE id % 11 = 0;
SELECT string_agg(id::text, ',') FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
SELECT pg_relation_size('a127_pt_p') / 8192 AS pages;
DELETE FROM a127_pt WHERE id < 1500;
VACUUM a127_pt;
SELECT relpages FROM pg_class WHERE relname = 'a127_pt_p';
SELECT string_agg(id::text, ',') FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
INSERT INTO a127_pt SELECT g, point((g * 3) % 100, (g * 59) % 100) FROM generate_series(3001, 3800) g;
VACUUM a127_pt;
SELECT relpages FROM pg_class WHERE relname = 'a127_pt_p';
SELECT pg_relation_size('a127_pt_p') / 8192 AS pages;
SELECT string_agg(id::text, ',') FROM a127_pt WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a127_pt ORDER BY p <-> point '(55,45)' LIMIT 40) s;
-- A range_ops index's pages the same way.
CREATE TABLE a127_rg (id int, r int4range) WITH (autovacuum_enabled = off);
INSERT INTO a127_rg SELECT g, int4range(g * 10, g * 10 + 5) FROM generate_series(1, 500) g;
CREATE INDEX a127_rg_r ON a127_rg USING gist (r);
UPDATE a127_rg SET r = int4range(id * 10 + 1, id * 10 + 3) WHERE id % 3 = 0;
DELETE FROM a127_rg WHERE id % 4 = 0;
INSERT INTO a127_rg SELECT g, int4range(g, g + 100) FROM generate_series(1, 300) g;
SELECT string_agg(id::text, ',' ORDER BY id) FROM a127_rg WHERE r && int4range(100, 400);
VACUUM a127_rg;
SELECT relpages FROM pg_class WHERE relname = 'a127_rg_r';
SELECT string_agg(id::text, ',' ORDER BY id) FROM a127_rg WHERE r && int4range(100, 400);
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a127_pt, a127_rg;
