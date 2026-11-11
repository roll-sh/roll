-- A table's TOAST table and index as relations of their own: names, renames under
-- allow_system_table_mods, REINDEX of them and its errors.
CREATE TABLE a56_tw (id int PRIMARY KEY, data text);
INSERT INTO a56_tw SELECT 1, string_agg(md5(g::text), '') FROM generate_series(1, 500) g;
-- Renaming needs allow_system_table_mods (the names hold the table's oid: hidden).
DO $$
DECLARE r record;
BEGIN
  SELECT reltoastrelid::regclass::text AS t FROM pg_class WHERE oid = 'a56_tw'::regclass INTO r;
  EXECUTE 'ALTER TABLE ' || r.t || ' RENAME TO a56_tw_toast';
EXCEPTION WHEN others THEN
  RAISE NOTICE 'got: % (%)', regexp_replace(sqlerrm, '[0-9]+', 'N'), sqlstate;
END $$;
SET allow_system_table_mods = on;
DO $$
DECLARE r record;
BEGIN
  SELECT reltoastrelid::regclass::text AS t FROM pg_class WHERE oid = 'a56_tw'::regclass INTO r;
  EXECUTE 'ALTER TABLE ' || r.t || ' RENAME TO a56_tw_toast';
  SELECT indexrelid::regclass::text AS i FROM pg_index
    WHERE indrelid = (SELECT oid FROM pg_class WHERE relname = 'a56_tw_toast') INTO r;
  EXECUTE 'ALTER INDEX ' || r.i || ' RENAME TO a56_tw_toast_idx';
END $$;
RESET allow_system_table_mods;
SELECT reltoastrelid::regclass FROM pg_class WHERE oid = 'a56_tw'::regclass;
SELECT c.relname, c.relkind, n.nspname
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relname LIKE 'a56_tw_toast%' ORDER BY 1;
SELECT indexrelid::regclass, indrelid::regclass FROM pg_index
  WHERE indrelid = 'pg_toast.a56_tw_toast'::regclass;
-- Renamed again, back and forth; a taken name; ALTER of the wrong kind.
ALTER INDEX pg_toast.a56_tw_toast_idx RENAME TO x;
SET allow_system_table_mods = on;
ALTER INDEX pg_toast.a56_tw_toast_idx RENAME TO a56_tw_i2;
ALTER TABLE pg_toast.a56_tw_i2 RENAME TO a56_tw_toast_idx;
ALTER TABLE pg_toast.a56_tw_toast RENAME TO a56_tw_toast_idx;
ALTER VIEW pg_toast.a56_tw_toast RENAME TO x;
ALTER SEQUENCE pg_toast.a56_tw_toast RENAME TO x;
ALTER INDEX pg_toast.a56_tw_toast RENAME TO a56_tw_t2;
ALTER INDEX pg_toast.a56_tw_t2 RENAME TO a56_tw_toast;
RESET allow_system_table_mods;
-- REINDEX of them.
REINDEX TABLE pg_toast.a56_tw_toast;
REINDEX INDEX pg_toast.a56_tw_toast_idx;
REINDEX TABLE pg_toast.a56_tw_toast_idx;
REINDEX INDEX pg_toast.a56_tw_toast;
REINDEX TABLE pg_toast.a56_tw_nothing;
REINDEX TABLE CONCURRENTLY pg_toast.a56_tw_toast;
REINDEX INDEX CONCURRENTLY pg_toast.a56_tw_toast_idx;
BEGIN;
REINDEX TABLE CONCURRENTLY pg_toast.a56_tw_toast;
ROLLBACK;
SELECT id, substr(data, 1, 10) FROM a56_tw;
-- Not the owner.
CREATE ROLE regress_a56_toast;
SET ROLE regress_a56_toast;
REINDEX TABLE pg_toast.a56_tw_toast;
REINDEX INDEX pg_toast.a56_tw_toast_idx;
RESET ROLE;
DROP ROLE regress_a56_toast;
DROP TABLE a56_tw;
REINDEX TABLE pg_toast.a56_tw_toast;
