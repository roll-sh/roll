-- GiST point_ops index scans return rows in the index's order (agent 114: its model is
-- kept across statements while the table stays as it was): after inserts, in a transaction
-- with writes of its own, and KNN order. (After an UPDATE and DELETE the model doesn't place
-- moved rows where PostgreSQL's tree has them, nor count its pages, as before this round:
-- rows compared by id, no KNN or size after it.)
CREATE TABLE a114_pt (id int, p point);
INSERT INTO a114_pt SELECT g, point((g * 37) % 100, (g * 91) % 100) FROM generate_series(1, 600) g;
CREATE INDEX a114_pt_p ON a114_pt USING gist (p);
ANALYZE a114_pt;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
SELECT string_agg(id::text, ',') FROM (SELECT id FROM a114_pt ORDER BY p <-> point '(50,50)' LIMIT 12) s;
INSERT INTO a114_pt SELECT g, point((g * 13) % 100, (g * 7) % 100) FROM generate_series(601, 700) g;
SELECT string_agg(id::text, ',') FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
UPDATE a114_pt SET p = point(20, 20) WHERE id % 50 = 0;
DELETE FROM a114_pt WHERE id % 7 = 0;
SELECT string_agg(id::text, ',' ORDER BY id) FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
BEGIN;
INSERT INTO a114_pt VALUES (5000, point(15, 15));
SELECT string_agg(id::text, ',' ORDER BY id) FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
ROLLBACK;
SELECT string_agg(id::text, ',' ORDER BY id) FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
VACUUM a114_pt;
SELECT string_agg(id::text, ',' ORDER BY id) FROM a114_pt WHERE p <@ box '(10,10),(30,30)';
RESET enable_seqscan;
RESET enable_bitmapscan;
DROP TABLE a114_pt;
