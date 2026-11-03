\set aid random(1, 10000)
\set delta random(-5000, 5000)
\set r random(1, 3)
BEGIN;
UPDATE acc SET abalance = abalance + :delta WHERE aid = :aid;
INSERT INTO hist (aid, delta, mtime) VALUES (:aid, :delta, now());
\if :r = 1
DELETE FROM hist WHERE hid = (SELECT min(hid) FROM hist);
\endif
END;
