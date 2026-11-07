-- A leaf VACUUM empties but can't delete (the rightmost child of a parent with other
-- children, _bt_lock_subtree_parent) is a leaf page to pgstatindex: its empty_pages are the
-- half-dead pages.
CREATE EXTENSION IF NOT EXISTS pgstattuple;
CREATE TABLE pe (id int PRIMARY KEY) WITH (autovacuum_enabled = false);
INSERT INTO pe SELECT generate_series(1, 200000);
-- The last leaf under the root's first child.
DELETE FROM pe WHERE id BETWEEN 103945 AND 104310;
VACUUM pe;
SELECT tree_level, internal_pages, leaf_pages, empty_pages, deleted_pages, avg_leaf_density, leaf_fragmentation
  FROM pgstatindex('pe_pkey');
-- Its left siblings emptied too: they go, it stays.
DELETE FROM pe WHERE id BETWEEN 100000 AND 103944;
VACUUM pe;
SELECT tree_level, internal_pages, leaf_pages, empty_pages, deleted_pages, avg_leaf_density, leaf_fragmentation
  FROM pgstatindex('pe_pkey');
SELECT pg_relation_size('pe_pkey') / 8192 AS pages;
DROP TABLE pe;
