-- GiST index scans mark the dead entries they return LP_DEAD (agent 139; gistkillitems, as
-- a plain unordered scan leaves each page, and at its end): later scans skip them, so KNN's
-- order of equally distant rows follows, and a full leaf page drops them before it would
-- split (gistprunepage), so the page count after more inserts does too.
CREATE TABLE a139_pt2 (id int, p point) WITH (autovacuum_enabled = off);
INSERT INTO a139_pt2 SELECT g, point((g * 37) % 100, (g * 91) % 100) FROM generate_series(1, 600) g;
CREATE INDEX a139_pt2_p ON a139_pt2 USING gist (p);
ANALYZE a139_pt2;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
INSERT INTO a139_pt2 SELECT g, point((g * 13) % 100, (g * 7) % 100) FROM generate_series(601, 700) g;
UPDATE a139_pt2 SET p = point(20, 20) WHERE id % 50 = 0;
DELETE FROM a139_pt2 WHERE id % 7 = 0;
-- (KNN scans don't mark: getNextNearest.)
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(73.3,71.1)' LIMIT 30) s;
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(20,20)' LIMIT 30) s;
SELECT string_agg(id::text, ',') FROM a139_pt2 WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(73.3,71.1)' LIMIT 30) s;
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(20,20)' LIMIT 30) s;
SELECT string_agg(id::text, ',') FROM a139_pt2 WHERE p <@ box '(0,0),(100,100)';
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(20,20)' LIMIT 30) s;
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(50,50)' LIMIT 40) s;
INSERT INTO a139_pt2 SELECT g, point((g * 17) % 100, (g * 29) % 100) FROM generate_series(1001, 1600) g;
SELECT pg_relation_size('a139_pt2_p') / 8192 AS pages;
SELECT string_agg(id::text, ',') FROM a139_pt2 WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a139_pt2 ORDER BY p <-> point '(20,20)' LIMIT 30) s;
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a139_pt2;
