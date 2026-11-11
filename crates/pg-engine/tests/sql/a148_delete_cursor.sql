-- A cursor declared just before a DELETE reads, through an index, the rows its snapshot
-- saw: the DELETE's deferred index entry deletes and tombstones aren't visible to it.
CREATE TABLE a148_cd (id int PRIMARY KEY, a int, t text);
CREATE INDEX a148_cd_a ON a148_cd (a);
INSERT INTO a148_cd SELECT g, g % 50, 'v' || g FROM generate_series(1, 3000) g;
ANALYZE a148_cd;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT id, t FROM a148_cd WHERE a = 7 ORDER BY a;
BEGIN;
DECLARE c1 CURSOR FOR SELECT count(*), sum(id) FROM a148_cd WHERE a = 7;
DECLARE c2 CURSOR FOR SELECT id, t FROM a148_cd WHERE a = 8 ORDER BY a, id;
DELETE FROM a148_cd WHERE a IN (7, 8);
FETCH ALL FROM c1;
FETCH 3 FROM c2;
SELECT count(*) FROM a148_cd WHERE a IN (7, 8);
DECLARE c3 CURSOR FOR SELECT count(*) FROM a148_cd WHERE id < 500;
DELETE FROM a148_cd WHERE id < 500;
FETCH ALL FROM c3;
FETCH ALL FROM c2;
SELECT count(*) FROM a148_cd WHERE id < 500;
SELECT count(*) FROM a148_cd;
COMMIT;
-- The same with a savepoint open, and rolled back to.
BEGIN;
DECLARE c4 CURSOR FOR SELECT count(*), sum(id) FROM a148_cd WHERE a = 9;
SAVEPOINT s;
DELETE FROM a148_cd WHERE a = 9;
FETCH ALL FROM c4;
ROLLBACK TO s;
SELECT count(*) FROM a148_cd WHERE a = 9;
DELETE FROM a148_cd WHERE a = 9;
SELECT count(*) FROM a148_cd WHERE a = 9;
COMMIT;
SELECT count(*), sum(id) FROM a148_cd;
RESET enable_seqscan;
RESET enable_bitmapscan;
SELECT count(*), sum(id) FROM a148_cd;
DROP TABLE a148_cd;
