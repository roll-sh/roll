-- ALTER TABLE odds and ends: views and catalogs as targets, system columns, renames, IF
-- EXISTS, persistence and foreign keys, the locks pg_locks shows, names of constraints and
-- expression indexes, COMMENT ON a domain's constraint, matview row locks.
CREATE TABLE gd_am_t (a int, b text);
CREATE VIEW gd_am_v AS SELECT a, b FROM gd_am_t;
ALTER TABLE gd_am_v ALTER COLUMN a SET DEFAULT 45;
ALTER TABLE gd_am_v ALTER COLUMN a DROP NOT NULL;
ALTER TABLE gd_am_v DROP COLUMN b;
ALTER TABLE gd_am_v SET (autovacuum_enabled = false);
ALTER TABLE gd_am_v RESET (autovacuum_enabled);
ALTER VIEW gd_am_v RESET (autovacuum_enabled);
INSERT INTO gd_am_v (b) VALUES ('x');
SELECT * FROM gd_am_t;
ALTER TABLE pg_class ALTER COLUMN relname SET NOT NULL;
ALTER TABLE gd_am_t DROP COLUMN xmin;
ALTER TABLE gd_am_t ALTER COLUMN xmin SET DATA TYPE int;
ALTER TABLE gd_am_t ALTER COLUMN a SET DEFAULT 'nope';
ALTER TABLE gd_am_t ADD PRIMARY KEY (nope);
ALTER TABLE IF EXISTS gd_am_nope RENAME CONSTRAINT x TO y;
ALTER TABLE IF EXISTS gd_am_nope SET SCHEMA gd_am_nosuch;
ALTER TABLE gd_am_t DETACH PARTITION gd_am_x FINALIZE;
CREATE TABLE pg_catalog.gd_am_sys ();
-- renaming an inherited column with ONLY
CREATE TABLE gd_am_rp (a int);
CREATE TABLE gd_am_rc () INHERITS (gd_am_rp);
ALTER TABLE ONLY gd_am_rp RENAME COLUMN a TO z;
DROP TABLE gd_am_rp CASCADE;
-- names of check constraints and expression indexes
CREATE TABLE gd_am_ck (s text, cap bool, CHECK (NOT (cap AND tableoid::regclass::text = 'x')));
CREATE TABLE gd_am_ck2 (s text, CHECK (ctid::text <> 'x'));
CREATE TYPE gd_am_pt AS (x int, y int);
CREATE TABLE gd_am_ck3 (p gd_am_pt CHECK ((p).x IS NOT NULL));
SELECT conrelid::regclass, conname FROM pg_constraint
 WHERE conrelid IN ('gd_am_ck'::regclass, 'gd_am_ck3'::regclass) ORDER BY 1;
CREATE INDEX ON gd_am_ck ((s::varchar));
CREATE INDEX ON gd_am_ck (ctid);
CREATE INDEX ON gd_am_ck ((ctid >= '(1000,0)'));
CREATE INDEX ON gd_am_ck (s) WHERE ctid >= '(1000,0)';
CREATE INDEX ON gd_am_ck ((s || s), (row(s, cap)));
CREATE UNIQUE INDEX gd_am_nnd ON gd_am_ck (s) NULLS NOT DISTINCT;
ALTER TABLE gd_am_ck ADD PRIMARY KEY USING INDEX gd_am_nnd;
SELECT indexrelid::regclass FROM pg_index WHERE indrelid = 'gd_am_ck'::regclass ORDER BY 1;
DROP TABLE gd_am_ck, gd_am_ck3;
DROP TYPE gd_am_pt;
-- persistence and foreign keys
CREATE UNLOGGED TABLE gd_am_u1 (a int PRIMARY KEY);
CREATE UNLOGGED TABLE gd_am_u2 (a int REFERENCES gd_am_u1);
ALTER TABLE gd_am_u2 SET LOGGED;
CREATE TABLE gd_am_l1 (a int PRIMARY KEY);
CREATE TABLE gd_am_l2 (a int REFERENCES gd_am_l1);
ALTER TABLE gd_am_l1 SET UNLOGGED;
DROP TABLE gd_am_u2, gd_am_u1, gd_am_l2, gd_am_l1;
-- the locks ALTER TABLE, CREATE TRIGGER, LOCK and RENAME take
CREATE TABLE gd_am_lk (f1 int PRIMARY KEY, f2 text);
CREATE TABLE gd_am_lk2 (f3 int PRIMARY KEY, f1 int);
CREATE VIEW gd_am_lkv AS SELECT * FROM gd_am_lk;
CREATE FUNCTION gd_am_locks() RETURNS TABLE (relname name, mode text) LANGUAGE sql AS $$
  SELECT c.relname, (ARRAY['AccessShareLock', 'RowShareLock', 'RowExclusiveLock',
      'ShareUpdateExclusiveLock', 'ShareLock', 'ShareRowExclusiveLock', 'ExclusiveLock',
      'AccessExclusiveLock'])[max(array_position(ARRAY['AccessShareLock', 'RowShareLock',
      'RowExclusiveLock', 'ShareUpdateExclusiveLock', 'ShareLock', 'ShareRowExclusiveLock',
      'ExclusiveLock', 'AccessExclusiveLock'], l.mode))]
    FROM pg_locks l JOIN pg_class c ON c.oid = l.relation
   WHERE l.locktype = 'relation' AND c.relname LIKE 'gd_am_lk%'
   GROUP BY c.relname ORDER BY 1 $$;
BEGIN;
ALTER TABLE gd_am_lk ALTER COLUMN f2 SET STATISTICS 150;
ALTER TABLE gd_am_lk CLUSTER ON gd_am_lk_pkey;
SELECT * FROM gd_am_locks();
ROLLBACK;
BEGIN;
ALTER TABLE gd_am_lk2 ADD FOREIGN KEY (f1) REFERENCES gd_am_lk (f1);
SELECT * FROM gd_am_locks();
ROLLBACK;
BEGIN;
LOCK TABLE gd_am_lkv IN EXCLUSIVE MODE;
SELECT * FROM gd_am_locks();
ROLLBACK;
BEGIN;
ALTER INDEX gd_am_lk_pkey RENAME TO gd_am_lk_pkey2;
SELECT * FROM gd_am_locks();
ROLLBACK;
BEGIN;
LOCK TABLE gd_am_lk_pkey;
ROLLBACK;
DROP VIEW gd_am_lkv;
DROP FUNCTION gd_am_locks();
DROP TABLE gd_am_lk2, gd_am_lk;
-- COMMENT ON CONSTRAINT ... ON DOMAIN
CREATE DOMAIN gd_am_dom AS int CONSTRAINT gd_am_dc CHECK (VALUE > 0);
COMMENT ON CONSTRAINT gd_am_dc ON DOMAIN gd_am_dom IS 'positive';
COMMENT ON CONSTRAINT nope ON DOMAIN gd_am_dom IS 'x';
COMMENT ON CONSTRAINT gd_am_dc ON DOMAIN gd_am_nodom IS 'x';
SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conname = 'gd_am_dc';
DROP DOMAIN gd_am_dom;
-- materialized views: no row locks; the unique index REFRESH CONCURRENTLY needs
CREATE MATERIALIZED VIEW gd_am_mv AS SELECT 1 AS i;
SELECT * FROM gd_am_mv FOR SHARE;
CREATE FUNCTION gd_am_dropidx() RETURNS bool LANGUAGE plpgsql AS $$
BEGIN EXECUTE 'DROP INDEX IF EXISTS public.gd_am_mv_idx'; RETURN true; END $$;
CREATE MATERIALIZED VIEW gd_am_mv2 AS SELECT 1 AS i WHERE gd_am_dropidx();
CREATE UNIQUE INDEX gd_am_mv_idx ON gd_am_mv2 (i);
REFRESH MATERIALIZED VIEW CONCURRENTLY gd_am_mv2;
DROP MATERIALIZED VIEW gd_am_mv, gd_am_mv2;
DROP FUNCTION gd_am_dropidx();
-- replica identity columns keep their NOT NULL
CREATE TABLE gd_am_ri (a int NOT NULL, b int NOT NULL);
CREATE UNIQUE INDEX gd_am_ri_idx ON gd_am_ri (a, b);
ALTER TABLE gd_am_ri REPLICA IDENTITY USING INDEX gd_am_ri_idx;
ALTER TABLE gd_am_ri ALTER b DROP NOT NULL;
DROP TABLE gd_am_ri;
DROP VIEW gd_am_v;
DROP TABLE gd_am_t;
