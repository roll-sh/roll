-- golden: raw-order
-- A scroll cursor's btree index scan turning in the middle of its range (agent 214): the
-- leaf page it is on returns its items in exactly the reverse order, and each page it then
-- enters is read afresh in the direction it moves (_bt_steppage, _bt_readpage), a posting
-- list's TIDs ascending either way. g = 8, 9, 10 have 135 rows each, in posting lists over
-- several leaf pages.
SET enable_seqscan = off;
SET enable_bitmapscan = off;
CREATE TABLE a214_cu (id int PRIMARY KEY, g int, v text) WITH (autovacuum_enabled = off);
INSERT INTO a214_cu SELECT i, i % 37, 'v' || i FROM generate_series(1, 5000) i;
CREATE INDEX a214_cu_g ON a214_cu (g);
-- Back over two pages, forward into the page it came from, back again.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id FROM a214_cu WHERE g BETWEEN 8 AND 10;
MOVE 250 FROM c;
FETCH BACKWARD 140 FROM c;
FETCH FORWARD 20 FROM c;
FETCH BACKWARD 30 FROM c;
FETCH ABSOLUTE 3 FROM c;
FETCH BACKWARD 5 FROM c;
COMMIT;
-- Two turns inside one page: exactly the rows passed, reversed.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id FROM a214_cu WHERE g = 9;
FETCH 3 FROM c;
FETCH BACKWARD 2 FROM c;
FETCH 4 FROM c;
COMMIT;
-- A filter over the scan; MOVE then FETCH RELATIVE.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id, v FROM a214_cu WHERE g BETWEEN 8 AND 10 AND id % 2 = 0;
MOVE 150 FROM c;
FETCH RELATIVE -60 FROM c;
FETCH BACKWARD 10 FROM c;
FETCH FORWARD 70 FROM c;
COMMIT;
-- Rows deleted in the transaction after DECLARE (the cursor doesn't see the deletes).
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id FROM a214_cu WHERE g BETWEEN 8 AND 10;
MOVE 250 FROM c;
DELETE FROM a214_cu WHERE id % 3 = 0;
FETCH BACKWARD 140 FROM c;
FETCH FORWARD 20 FROM c;
ROLLBACK;
-- A backward index scan (ORDER BY g DESC) turning forward.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id FROM a214_cu WHERE g BETWEEN 8 AND 10 ORDER BY g DESC;
MOVE 250 FROM c;
FETCH BACKWARD 140 FROM c;
FETCH 20 FROM c;
COMMIT;
-- A unique index: no posting lists, every turn as before.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id FROM a214_cu WHERE id BETWEEN 100 AND 900;
MOVE 500 FROM c;
FETCH BACKWARD 3 FROM c;
FETCH LAST FROM c;
FETCH BACKWARD 2 FROM c;
COMMIT;
DROP TABLE a214_cu;
