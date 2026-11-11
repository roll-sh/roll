-- max_prepared_transactions: 5
-- Two-phase commit: PREPARE TRANSACTION, COMMIT / ROLLBACK PREPARED, pg_prepared_xacts,
-- the prepared transaction's locks, and every refusal.
DROP ROLE IF EXISTS regress_a71_2pc;
DROP ROLE IF EXISTS regress_a71_2pc_other;
CREATE ROLE regress_a71_2pc;
CREATE ROLE regress_a71_2pc_other;
CREATE TABLE tp (id int PRIMARY KEY, v text);
GRANT ALL ON tp TO regress_a71_2pc, regress_a71_2pc_other;
INSERT INTO tp VALUES (1, 'a'), (2, 'b');
SHOW max_prepared_transactions;

-- Outside a block: a warning, and no PREPARE.
PREPARE TRANSACTION 'a71_none';
SELECT count(*) FROM pg_prepared_xacts;

-- A prepared transaction's writes are not seen, its locks are held.
BEGIN;
UPDATE tp SET v = 'x' WHERE id = 1;
INSERT INTO tp VALUES (3, 'c');
PREPARE TRANSACTION 'a71_one';
SELECT * FROM tp ORDER BY id;
SELECT gid, owner FROM pg_prepared_xacts ORDER BY gid;
SELECT transaction::text::bigint > 0 AS has_xid, prepared <= now() AS ok FROM pg_prepared_xacts;
SELECT locktype, mode, granted, pid IS NULL AS no_pid, virtualtransaction LIKE '-1/%' AS dummy
  FROM pg_locks WHERE pid IS NULL AND locktype IN ('relation', 'transactionid') ORDER BY 1, 2;
SELECT * FROM tp WHERE id = 1 FOR UPDATE NOWAIT;
BEGIN;
LOCK TABLE tp IN ACCESS EXCLUSIVE MODE NOWAIT;
ROLLBACK;
UPDATE tp SET v = 'free' WHERE id = 2;

-- The gid is taken (and checked before LISTEN / NOTIFY).
BEGIN;
NOTIFY a71_chan;
PREPARE TRANSACTION 'a71_one';
SELECT gid FROM pg_prepared_xacts ORDER BY gid;

-- Errors finishing one.
COMMIT PREPARED 'a71_nosuch';
ROLLBACK PREPARED 'a71_nosuch';
BEGIN;
COMMIT PREPARED 'a71_one';
ROLLBACK;
BEGIN;
ROLLBACK PREPARED 'a71_one';
ROLLBACK;
SET ROLE regress_a71_2pc;
COMMIT PREPARED 'a71_one';
RESET ROLE;
COMMIT PREPARED 'a71_one';
COMMIT PREPARED 'a71_one';
SELECT * FROM tp ORDER BY id;
SELECT count(*) FROM pg_prepared_xacts;

-- Prepared by a role: only it or a superuser finishes it.
SET ROLE regress_a71_2pc;
BEGIN;
DELETE FROM tp WHERE id = 3;
PREPARE TRANSACTION 'a71_role';
SELECT gid, owner FROM pg_prepared_xacts;
SET ROLE regress_a71_2pc_other;
ROLLBACK PREPARED 'a71_role';
SET ROLE regress_a71_2pc;
ROLLBACK PREPARED 'a71_role';
RESET ROLE;
SELECT * FROM tp ORDER BY id;

-- Refusals.
BEGIN;
PREPARE TRANSACTION 'gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg';
BEGIN;
LISTEN a71_chan;
PREPARE TRANSACTION 'a71_listen';
BEGIN;
NOTIFY a71_chan, 'x';
PREPARE TRANSACTION 'a71_notify';
BEGIN;
UNLISTEN *;
PREPARE TRANSACTION 'a71_unlisten';
-- (Never listening, UNLISTEN queued nothing: it prepared.)
ROLLBACK PREPARED 'a71_unlisten';
BEGIN ISOLATION LEVEL REPEATABLE READ;
SELECT pg_export_snapshot() IS NOT NULL;
PREPARE TRANSACTION 'a71_snap';
BEGIN;
DECLARE c CURSOR WITH HOLD FOR SELECT * FROM tp;
PREPARE TRANSACTION 'a71_hold';
BEGIN;
CREATE TEMP TABLE a71_tmp (a int);
PREPARE TRANSACTION 'a71_temp';
SELECT pg_advisory_lock(71);
BEGIN;
SELECT pg_advisory_xact_lock(71);
PREPARE TRANSACTION 'a71_both';
SELECT pg_advisory_unlock(71);
-- A failed block: PREPARE rolls it back.
BEGIN;
SELECT 1/0;
PREPARE TRANSACTION 'a71_failed';
SELECT count(*) FROM pg_prepared_xacts;

-- A deferred constraint is checked at PREPARE.
CREATE TABLE tp_ref (id int REFERENCES tp DEFERRABLE INITIALLY DEFERRED);
BEGIN;
INSERT INTO tp_ref VALUES (99);
PREPARE TRANSACTION 'a71_deferred';
SELECT count(*) FROM pg_prepared_xacts;

-- DDL, savepoints and SET LOCAL in a prepared transaction; a non-holdable cursor closes.
BEGIN;
CREATE TABLE tp_new (a int);
INSERT INTO tp_new VALUES (1);
SAVEPOINT s1;
INSERT INTO tp_new VALUES (2);
ROLLBACK TO s1;
SAVEPOINT s2;
INSERT INTO tp_new VALUES (3);
SET LOCAL work_mem = '2MB';
SET search_path = public, pg_catalog;
DROP TABLE tp_ref;
DECLARE c2 CURSOR FOR SELECT 1;
PREPARE TRANSACTION 'a71_ddl';
FETCH c2;
SHOW work_mem;
SHOW search_path;
RESET search_path;
SELECT * FROM tp_new;
SELECT count(*) FROM pg_class WHERE relname = 'tp_ref';
COMMIT PREPARED 'a71_ddl';
SELECT * FROM tp_new ORDER BY a;
SELECT count(*) FROM pg_class WHERE relname = 'tp_ref';

-- Read-only and row-lock-only transactions prepare too; the registry has room for five.
BEGIN;
SELECT * FROM tp WHERE id = 1 FOR SHARE;
PREPARE TRANSACTION 'a71_p1';
BEGIN;
SELECT * FROM tp WHERE id = 1 FOR SHARE;
PREPARE TRANSACTION 'a71_p2';
BEGIN;
SELECT 1;
PREPARE TRANSACTION 'a71_p3';
BEGIN;
PREPARE TRANSACTION 'a71_p4';
BEGIN;
PREPARE TRANSACTION 'a71_p5';
BEGIN;
PREPARE TRANSACTION 'a71_p6';
SELECT gid FROM pg_prepared_xacts ORDER BY gid;
SELECT * FROM tp WHERE id = 1 FOR UPDATE NOWAIT;
SELECT * FROM tp WHERE id = 1 FOR KEY SHARE NOWAIT;
-- (pg_prepared_xact()'s order: a finished one's slot takes the last one.)
ROLLBACK PREPARED 'a71_p2';
SELECT gid FROM pg_prepared_xacts;
ROLLBACK PREPARED 'a71_p1';
ROLLBACK PREPARED 'a71_p3';
COMMIT PREPARED 'a71_p4';
COMMIT PREPARED 'a71_p5';
SELECT count(*) FROM pg_prepared_xacts;

DROP TABLE tp, tp_new;
DROP ROLE regress_a71_2pc;
DROP ROLE regress_a71_2pc_other;
