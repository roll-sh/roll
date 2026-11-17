-- max_prepared_transactions: 2
-- MultiXact: a row lock leaves a lock-only xmax on the version (the locker's xid, a
-- MultiXactId once another subtransaction locks it too), which stays after the locker ends;
-- pgrowlocks and pg_get_multixact_members read it. Ids are shown relative to the session's own.
CREATE EXTENSION pgrowlocks;
CREATE TABLE mx (id int PRIMARY KEY, v int);
INSERT INTO mx SELECT g, g FROM generate_series(1, 5) g;
BEGIN;
SELECT id FROM mx WHERE id = 1 FOR KEY SHARE;
SELECT id FROM mx WHERE id = 2 FOR SHARE;
SELECT id FROM mx WHERE id = 3 FOR NO KEY UPDATE;
SELECT id FROM mx WHERE id = 4 FOR UPDATE;
SELECT id, xmax = pg_current_xact_id()::xid AS mine, xmax = '0' AS none FROM mx ORDER BY id;
SELECT locked_row, locker = pg_current_xact_id()::xid AS mine, multi, modes, pids[1] = pg_backend_pid() AS own_pid FROM pgrowlocks('mx') ORDER BY 1;
-- A weaker lock than the one held changes nothing; a stronger one takes its place.
SELECT id FROM mx WHERE id = 4 FOR KEY SHARE;
SELECT id FROM mx WHERE id = 1 FOR UPDATE;
SELECT locked_row, multi, modes FROM pgrowlocks('mx') ORDER BY 1;
-- In a subtransaction (an xid of its own) a stronger lock makes a multi of both.
SAVEPOINT s1;
SELECT id FROM mx WHERE id = 2 FOR UPDATE;
SELECT id FROM mx WHERE id = 4 FOR SHARE;
SELECT locked_row, multi, modes, xids[1] = pg_current_xact_id()::xid AS first_is_top FROM pgrowlocks('mx') ORDER BY 1;
SELECT m.mode, m.xid = pg_current_xact_id()::xid AS top FROM mx, pg_get_multixact_members(mx.xmax) m WHERE mx.id = 2;
-- A multi's member again in the same mode: the same multi.
CREATE TEMP TABLE before AS SELECT xmax AS x FROM mx WHERE id = 2;
SELECT id FROM mx WHERE id = 2 FOR UPDATE;
SELECT mx.xmax = before.x AS same FROM mx, before WHERE id = 2;
ROLLBACK TO s1;
-- The multi stays; its subtransaction member ended, the top one still runs.
SELECT locked_row, multi, modes FROM pgrowlocks('mx') ORDER BY 1;
-- Locked, then updated by the same transaction: the old version's xmax is the update, the
-- new version's the lock (FOR KEY SHARE: what an update leaves of it).
UPDATE mx SET v = v + 10 WHERE id = 3;
UPDATE mx SET v = v + 10 WHERE id = 5;
SELECT id, v, xmax = pg_current_xact_id()::xid AS mine, xmax = '0' AS none FROM mx ORDER BY id;
SELECT locked_row, multi, modes FROM pgrowlocks('mx') ORDER BY 1;
COMMIT;
-- After the lockers end the xmax stays, no longer a lock.
SELECT id, xmax = '0' AS none FROM mx ORDER BY id;
SELECT count(*) FROM pgrowlocks('mx');
-- A new lock replaces a stale xmax with the new locker alone.
BEGIN;
SELECT id FROM mx WHERE id = 2 FOR KEY SHARE;
SELECT id, xmax = pg_current_xact_id()::xid AS mine FROM mx WHERE id = 2;
SELECT locked_row, multi, modes FROM pgrowlocks('mx') ORDER BY 1;
-- A prepared transaction keeps its lock; its backend is gone (pid 0).
PREPARE TRANSACTION 'a249';
SELECT locked_row, multi, modes, pids FROM pgrowlocks('mx') ORDER BY 1;
ROLLBACK PREPARED 'a249';
SELECT count(*) FROM pgrowlocks('mx');
-- Foreign key checks lock the referenced row FOR KEY SHARE.
CREATE TABLE mxc (id int, p int REFERENCES mx);
BEGIN;
INSERT INTO mxc VALUES (1, 1);
SELECT locked_row, multi, modes FROM pgrowlocks('mx') ORDER BY 1;
SAVEPOINT s2;
SELECT id FROM mx WHERE id = 1 FOR NO KEY UPDATE;
SELECT locked_row, multi, modes FROM pgrowlocks('mx') ORDER BY 1;
COMMIT;
-- ON CONFLICT DO UPDATE locks the row it updates: its new version keeps the lock.
BEGIN;
INSERT INTO mx VALUES (5, 0) ON CONFLICT (id) DO UPDATE SET v = excluded.v RETURNING id, v, xmax = pg_current_xact_id()::xid AS mine;
SELECT id, xmax = pg_current_xact_id()::xid AS mine FROM mx WHERE id = 5;
COMMIT;
SELECT * FROM pg_get_multixact_members('0');
SELECT * FROM pg_get_multixact_members('4000000000');
SELECT mxid_age('0');
DROP TABLE mxc;
DROP TABLE mx;
