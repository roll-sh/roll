-- TID scans fetch the rows at their TIDs (agent 135): equality, = ANY with duplicates
-- and NULLs, TIDs of no row, of an updated row's old and new versions, of deleted rows,
-- of the transaction's own rows; UPDATE / DELETE by ctid, prepared, and a join by ctid.
CREATE TABLE a135_t (id int PRIMARY KEY, v text, n int);
INSERT INTO a135_t SELECT i, 'v' || i, i % 7 FROM generate_series(1, 2000) i;
VACUUM ANALYZE a135_t;
EXPLAIN (COSTS OFF) SELECT id FROM a135_t WHERE ctid = '(3,5)';
SELECT ctid, id, v FROM a135_t WHERE ctid = '(3,5)';
SELECT ctid, id FROM a135_t WHERE ctid = '(3,5)' AND n = 0;
SELECT ctid, id FROM a135_t WHERE ctid = '(3,5)' AND n = 1;
SELECT ctid, id FROM a135_t WHERE ctid = '(900,1)';
SELECT ctid, id FROM a135_t WHERE ctid = '(0,0)';
SELECT ctid, id FROM a135_t WHERE ctid = '(2,300)';
EXPLAIN (COSTS OFF) SELECT id FROM a135_t WHERE ctid = ANY (ARRAY['(5,1)', '(0,2)', '(5,1)', NULL, '(1,7)']::tid[]);
SELECT ctid, id FROM a135_t WHERE ctid = ANY (ARRAY['(5,1)', '(0,2)', '(5,1)', NULL, '(1,7)']::tid[]);
SELECT ctid, id FROM a135_t WHERE ctid = '(4,2)' OR ctid = '(0,3)' OR ctid = '(4,2)';
SELECT ctid, id FROM a135_t WHERE ctid = NULL::tid;
SELECT ctid, id FROM a135_t WHERE ctid = ANY (NULL::tid[]);
SELECT count(*) FROM a135_t WHERE ctid = ANY (ARRAY(SELECT ('(' || b || ',' || o || ')')::tid FROM generate_series(0, 12) b, generate_series(1, 200, 37) o));
-- an updated row: its new version at a new TID, the old one gone
UPDATE a135_t SET v = 'changed' WHERE id = 20;
SELECT ctid, id, v FROM a135_t WHERE id = 20;
SELECT ctid, id, v FROM a135_t WHERE ctid = (SELECT ctid FROM a135_t WHERE id = 20);
SELECT ctid, id, v FROM a135_t WHERE ctid = '(0,20)';
-- within a transaction: its own updates, deletes and inserts
BEGIN;
UPDATE a135_t SET v = 'again' WHERE id = 21;
SELECT ctid, id, v FROM a135_t WHERE ctid = (SELECT ctid FROM a135_t WHERE id = 21);
SELECT ctid, id, v FROM a135_t WHERE ctid = '(0,21)';
DELETE FROM a135_t WHERE id = 22;
SELECT ctid, id FROM a135_t WHERE ctid = '(0,22)';
INSERT INTO a135_t VALUES (5000, 'new', 1);
SELECT ctid, id, v FROM a135_t WHERE ctid = (SELECT ctid FROM a135_t WHERE id = 5000);
ROLLBACK;
SELECT ctid, id, v FROM a135_t WHERE ctid = '(0,21)';
SELECT ctid, id FROM a135_t WHERE ctid = '(0,22)';
-- UPDATE and DELETE by ctid (as postgres_fdw sends them), prepared
PREPARE a135_upd (tid, text) AS UPDATE a135_t SET v = $2 WHERE ctid = $1 RETURNING id, v;
EXECUTE a135_upd ('(1,10)', 'fdw1');
EXECUTE a135_upd ('(1,10)', 'fdw2');
PREPARE a135_del (tid) AS DELETE FROM a135_t WHERE ctid = $1 RETURNING id;
EXECUTE a135_del ('(2,4)');
EXECUTE a135_del ('(2,4)');
SELECT id, v FROM a135_t WHERE v LIKE 'fdw%' ORDER BY id;
-- a join reading the inner side by the outer side's TIDs
CREATE TABLE a135_c (c tid, k int);
INSERT INTO a135_c VALUES ('(0,5)', 1), ('(3,9)', 2), ('(77,1)', 3), (NULL, 4), ('(0,5)', 5);
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SELECT c.k, t.id FROM a135_c c JOIN a135_t t ON t.ctid = c.c ORDER BY c.k;
RESET enable_hashjoin;
RESET enable_mergejoin;
DROP TABLE a135_c;
DROP TABLE a135_t;
