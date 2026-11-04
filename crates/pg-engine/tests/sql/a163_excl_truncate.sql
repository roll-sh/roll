-- Exclusion constraints checked against a transaction's own rows after a TRUNCATE in that
-- transaction (agent 163): the rows it inserted after the TRUNCATE conflict, the ones the
-- TRUNCATE removed (committed or its own) don't, and rows it deleted again don't either.
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE TABLE a163_bk (id serial, room int, during tsrange, EXCLUDE USING gist (during WITH &&));
CREATE TABLE a163_rm (id serial, room int, during tsrange, EXCLUDE USING gist (room WITH =, during WITH &&));
INSERT INTO a163_bk (during) VALUES (tsrange('2026-01-01 00:00', '2026-01-01 10:00'));
INSERT INTO a163_rm (room, during) VALUES (1, tsrange('2026-01-01 00:00', '2026-01-01 10:00'));
BEGIN;
TRUNCATE a163_bk;
-- (The committed row is gone for this transaction: no conflict with it.)
INSERT INTO a163_bk (during) SELECT tsrange(timestamp '2026-01-01' + g * interval '1 hour',
  timestamp '2026-01-01' + (g + 1) * interval '1 hour') FROM generate_series(1, 300) g;
SELECT count(*) FROM a163_bk;
-- A row of its own after the TRUNCATE.
INSERT INTO a163_bk (during) VALUES (tsrange('2026-01-01 03:30', '2026-01-01 04:30'));
ROLLBACK;
BEGIN;
INSERT INTO a163_bk (during) VALUES (tsrange('2026-02-01 00:00', '2026-02-01 01:00'));
TRUNCATE a163_bk;
-- (Its own row from before the TRUNCATE is gone too.)
INSERT INTO a163_bk (during) VALUES (tsrange('2026-02-01 00:30', '2026-02-01 00:45'));
INSERT INTO a163_bk (during) SELECT tsrange(timestamp '2026-03-01' + g * interval '1 hour',
  timestamp '2026-03-01' + (g + 1) * interval '1 hour') FROM generate_series(1, 200) g;
DELETE FROM a163_bk WHERE during && tsrange(timestamp '2026-03-01' + interval '50 hours', timestamp '2026-03-01' + interval '52 hours');
INSERT INTO a163_bk (during) VALUES (tsrange('2026-03-03 02:30', '2026-03-03 03:30'));
SAVEPOINT s;
INSERT INTO a163_bk (during) VALUES (tsrange('2026-03-01 10:30', '2026-03-01 11:30'));
ROLLBACK TO s;
-- One statement's rows conflicting among themselves.
INSERT INTO a163_bk (during) VALUES (tsrange('2026-04-01 00:00', '2026-04-01 02:00')),
  (tsrange('2026-04-01 01:00', '2026-04-01 03:00'));
ROLLBACK TO s;
COMMIT;
SELECT count(*), min(lower(during)), max(upper(during)) FROM a163_bk;
-- With a leading equality.
BEGIN;
TRUNCATE a163_rm;
INSERT INTO a163_rm (room, during) SELECT g % 5, tsrange(timestamp '2026-01-01' + g * interval '1 hour',
  timestamp '2026-01-01' + (g + 1) * interval '1 hour') FROM generate_series(1, 200) g;
INSERT INTO a163_rm (room, during) VALUES (7, tsrange('2026-01-01 00:00', '2026-01-01 10:00'));
SAVEPOINT s;
INSERT INTO a163_rm (room, during) VALUES (2, tsrange('2026-01-01 06:30', '2026-01-01 07:30'));
ROLLBACK TO s;
COMMIT;
SELECT room, count(*) FROM a163_rm GROUP BY room ORDER BY room;
DROP TABLE a163_bk, a163_rm;
