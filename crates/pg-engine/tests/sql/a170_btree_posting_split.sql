-- Heap TIDs inside an equal key's posting list (rows put into space VACUUM freed, after
-- deduplication merged their key's entries) split the list (_bt_swap_posting): the list keeps
-- its size, its last TID goes in as the new entry. Then a non-HOT UPDATE of every ninth key.
CREATE EXTENSION IF NOT EXISTS pgstattuple;
CREATE TABLE ps (id bigserial PRIMARY KEY, ref int, note text) WITH (autovacuum_enabled = false);
CREATE INDEX ps_ref ON ps (ref);
INSERT INTO ps (ref, note) SELECT g % 150, repeat('n', g % 40) FROM generate_series(1, 60000) g;
DELETE FROM ps WHERE id % 3 = 0;
VACUUM ps;
SELECT * FROM pgstattuple('ps_ref');
INSERT INTO ps (ref, note) SELECT g % 21, 'again' FROM generate_series(1, 12000) g;
SELECT leaf_pages, avg_leaf_density, leaf_fragmentation FROM pgstatindex('ps_ref');
SELECT tuple_count, tuple_len, dead_tuple_count, free_space FROM pgstattuple('ps_ref');
UPDATE ps SET note = note || '!' WHERE ref % 9 = 0;
SELECT leaf_pages, avg_leaf_density, leaf_fragmentation FROM pgstatindex('ps_ref');
SELECT tuple_count, tuple_len, dead_tuple_count, free_space FROM pgstattuple('ps_ref');
SELECT pg_relation_size('ps_ref') / 8192 AS pages, pg_relation_size('ps') / 8192 AS heap;
SELECT ctid, id FROM ps WHERE id IN (3, 60001, 72000) ORDER BY id;
DROP TABLE ps;
