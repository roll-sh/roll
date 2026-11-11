-- TID scans read as many rows as TIDs (agent 143): every version's TID names its row
-- (`Heap::key_at`), so a TID of an older version, of a dead or unused line pointer, or of
-- no tuple at all gives nothing without reading the table, and a row's current version
-- comes out at its own TID.
CREATE TABLE a143_t (id int, v text);
INSERT INTO a143_t SELECT g, 'v' || g FROM generate_series(1, 400) g;
SELECT ctid, id FROM a143_t WHERE id IN (1, 2, 3) ORDER BY id;
UPDATE a143_t SET v = 'w1' WHERE id = 1;
SELECT ctid, id, v FROM a143_t WHERE id = 1;
-- the old version's TID, the new one's, a TID past the page's tuples, a block past the end
SELECT id, v FROM a143_t WHERE ctid = '(0,1)';
SELECT id, v FROM a143_t WHERE ctid = (SELECT ctid FROM a143_t WHERE id = 1);
SELECT id, v FROM a143_t WHERE ctid = '(0,999)';
SELECT id, v FROM a143_t WHERE ctid = '(9999,1)';
SELECT ctid, id, v FROM a143_t WHERE ctid = ANY (ARRAY['(0,1)', '(0,2)', '(0,3)', '(0,999)']::tid[]) ORDER BY ctid;
SELECT ctid, id FROM a143_t WHERE ctid = ANY (ARRAY['(0,1)', '(0,1)', '(0,3)', '(0,3)', NULL, '(0,999)']::tid[]) ORDER BY ctid;
DELETE FROM a143_t WHERE id = 2;
SELECT id FROM a143_t WHERE ctid = '(0,2)';
SELECT id FROM a143_t WHERE ctid = '(0,2)' OR ctid = '(0,3)';
VACUUM a143_t;
SELECT id FROM a143_t WHERE ctid = '(0,1)' OR ctid = '(0,2)' OR ctid = '(0,3)';
INSERT INTO a143_t VALUES (1000, 'reused');
SELECT ctid, id FROM a143_t WHERE id = 1000;
SELECT id, v FROM a143_t WHERE ctid IN ('(0,1)', '(0,2)');
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT id FROM a143_t WHERE ctid = ANY (ARRAY['(0,1)', '(0,3)', '(5,1)']::tid[]);
UPDATE a143_t SET v = 'by tid' WHERE ctid = '(0,3)' RETURNING id, v;
UPDATE a143_t SET v = 'stale' WHERE ctid = '(0,3)' RETURNING id, v;
DROP TABLE a143_t;
