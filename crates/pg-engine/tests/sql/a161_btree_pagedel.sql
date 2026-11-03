-- VACUUM deletes the btree pages it empties (_bt_pagedel) and, once every transaction is
-- past them, records them in the index's free space map; later splits reuse them.
CREATE EXTENSION IF NOT EXISTS pgstattuple;
CREATE TABLE pd (id int PRIMARY KEY, v int) WITH (autovacuum_enabled = false);
CREATE INDEX pd_v ON pd (v);
INSERT INTO pd SELECT g, g % 50 FROM generate_series(1, 40000) g;
CREATE VIEW pd_ix AS
  SELECT c.relname, pg_relation_size(c.oid) / 8192 AS main, pg_relation_size(c.oid, 'fsm') / 8192 AS fsm,
         s.tree_level, s.internal_pages, s.leaf_pages, s.empty_pages, s.deleted_pages
  FROM pg_class c, LATERAL pgstatindex(c.oid) s WHERE c.relname IN ('pd_pkey', 'pd_v') ORDER BY 1;
SELECT * FROM pd_ix;
-- The first half's leaves empty: deleted, not yet free (their safexid is the next xid).
DELETE FROM pd WHERE id <= 20000;
VACUUM pd;
SELECT * FROM pd_ix;
SELECT relname, relpages FROM pg_class WHERE relname IN ('pd_pkey', 'pd_v') ORDER BY 1;
-- Nothing to remove, and no transaction since: the cleanup scan still can't free them.
VACUUM pd;
SELECT * FROM pd_ix;
-- A transaction later they are free: into the free space map.
SELECT txid_current() > 0;
VACUUM pd;
SELECT * FROM pd_ix;
-- Inserts into the deleted key space split onto the freed pages; the file doesn't grow.
INSERT INTO pd SELECT g, 7 FROM generate_series(1, 12000) g;
SELECT * FROM pd_ix;
SELECT ctid, id FROM pd WHERE id IN (1, 6000, 12000) ORDER BY id;
-- A middle stretch emptied with VACUUM's own dead tuples, and an only child's parent.
DELETE FROM pd WHERE id BETWEEN 25000 AND 32000;
VACUUM pd;
SELECT * FROM pd_ix;
SELECT txid_current() > 0;
VACUUM pd;
SELECT * FROM pd_ix;
INSERT INTO pd SELECT g, g % 50 FROM generate_series(40001, 52000) g;
SELECT * FROM pd_ix;
-- Kept across a clean restart is checked by the size-model scripts; here, a rewrite.
REINDEX TABLE pd;
SELECT * FROM pd_ix;
DROP VIEW pd_ix;
DROP TABLE pd;
