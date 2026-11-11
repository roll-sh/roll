-- golden: raw-order
-- A btree index scan that ran off an end of its range starts again from that end when a
-- scroll cursor turns back (_bt_first): read backward afresh, each posting list's TIDs come
-- in ascending heap order. The 135 entries of g = 9 lie in two posting lists on two leaf
-- pages, so turning back at the end reads 4893, 4930, 4967 before 9, 46, ...
SET enable_seqscan = off;
SET enable_bitmapscan = off;
CREATE TABLE a206_u (id int PRIMARY KEY, g int, v text);
INSERT INTO a206_u SELECT i, i % 37, 'v' || i FROM generate_series(1, 5000) i;
CREATE INDEX a206_u_g ON a206_u (g);
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id, g FROM a206_u WHERE g = 9;
FETCH 3 FROM c;
FETCH BACKWARD 2 FROM c;
FETCH ABSOLUTE 50 FROM c;
FETCH LAST FROM c;
FETCH BACKWARD 3 FROM c;
FETCH FORWARD 2 FROM c;
COMMIT;
-- Turning after the end, then running off the start and turning again: a fresh forward scan.
BEGIN;
DECLARE d SCROLL CURSOR FOR SELECT id FROM a206_u WHERE g = 9;
MOVE ALL FROM d;
FETCH BACKWARD 4 FROM d;
MOVE BACKWARD ALL FROM d;
FETCH 3 FROM d;
FETCH ABSOLUTE -2 FROM d;
COMMIT;
-- Not at the end: stepping back over the rows passed.
BEGIN;
DECLARE e SCROLL CURSOR FOR SELECT id FROM a206_u WHERE g = 9;
MOVE 135 FROM e;
FETCH BACKWARD 3 FROM e;
COMMIT;
-- A backward scan turning at its end reads forward afresh from the low end.
BEGIN;
DECLARE f SCROLL CURSOR FOR SELECT id, g FROM a206_u WHERE g BETWEEN 8 AND 9 ORDER BY g DESC;
MOVE ALL FROM f;
FETCH BACKWARD 4 FROM f;
COMMIT;
BEGIN;
DECLARE h SCROLL CURSOR FOR SELECT id FROM a206_u WHERE g = 10 ORDER BY g;
FETCH LAST FROM h;
COMMIT;
-- After deletes and updates (agent 203's case).
DELETE FROM a206_u WHERE id % 11 = 0 AND id < 3000;
UPDATE a206_u SET g = 7 WHERE id IN (100, 101, 102, 4999);
BEGIN;
DECLARE k SCROLL CURSOR FOR SELECT id, g FROM a206_u WHERE g = 9;
FETCH ABSOLUTE 50 FROM k;
FETCH LAST FROM k;
FETCH RELATIVE -2 FROM k;
COMMIT;
DROP TABLE a206_u;
