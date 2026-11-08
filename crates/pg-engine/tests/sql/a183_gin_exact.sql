-- GIN bitmap scans whose structure's selection is exact (tsvector AND / OR of lexemes, an
-- array's @> / &&): counted without reading the rows while nothing was written to the
-- table since, read and checked once something was, and in a transaction with its own writes.
CREATE TABLE ge (id int PRIMARY KEY, tags int[], tsv tsvector);
INSERT INTO ge SELECT g, ARRAY[g % 10, g % 7, 100 + g % 3],
  to_tsvector('simple', 'w' || (g % 50) || ' x' || (g % 9) || ' common') FROM generate_series(1, 3000) g;
CREATE INDEX ge_tags ON ge USING gin (tags);
CREATE INDEX ge_tsv ON ge USING gin (tsv);
ANALYZE ge;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 | w7');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'w1:*');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & !w3');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3:A');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 <-> common');
SELECT count(*) FROM ge WHERE tags @> ARRAY[3];
SELECT count(*) FROM ge WHERE tags @> ARRAY[3, 4];
SELECT count(*) FROM ge WHERE tags && ARRAY[3, 101];
SELECT count(*) FROM ge WHERE tags @> ARRAY[3, NULL];
SELECT count(*) FROM ge WHERE tags && ARRAY[3, NULL];
SELECT count(*) FROM ge WHERE tags @> ARRAY[3] AND tsv @@ to_tsquery('simple', 'x3');
SELECT count(*) FROM ge WHERE tags @> ARRAY[3] OR tsv @@ to_tsquery('simple', 'x3');
SELECT count(*), sum(id) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common') AND id > 1500;
SELECT id FROM ge WHERE tags @> ARRAY[3, 4] ORDER BY id LIMIT 5;
-- The transaction's own writes.
BEGIN;
DELETE FROM ge WHERE id % 9 = 3 AND id < 1000;
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
INSERT INTO ge VALUES (5000, ARRAY[3], to_tsvector('simple', 'x3 common'));
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tags @> ARRAY[3];
ROLLBACK;
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tags @> ARRAY[3];
-- Rows written again and deleted since the structure read them.
UPDATE ge SET tsv = to_tsvector('simple', 'other'), tags = ARRAY[0] WHERE id <= 30;
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tags @> ARRAY[3];
DELETE FROM ge WHERE id BETWEEN 31 AND 60;
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tags && ARRAY[3, 101];
INSERT INTO ge VALUES (6000, ARRAY[3, 4], to_tsvector('simple', 'x3 common'));
SELECT count(*) FROM ge WHERE tsv @@ to_tsquery('simple', 'x3 & common');
SELECT count(*) FROM ge WHERE tags @> ARRAY[3, 4];
-- jsonb's key tests (`?`, `?|`, `?&`: the top level's keys and string elements).
CREATE TABLE gj (id int, j jsonb);
INSERT INTO gj SELECT g, CASE g % 5
    WHEN 0 THEN jsonb_build_object('k' || (g % 7), 1, 'n', jsonb_build_object('deep' || (g % 3), true))
    WHEN 1 THEN jsonb_build_array('k' || (g % 7), 'x', jsonb_build_object('k1', 1))
    WHEN 2 THEN to_jsonb('k' || (g % 7))
    WHEN 3 THEN jsonb_build_object('a', jsonb_build_array('k1', 'k2'))
    ELSE NULL END FROM generate_series(1, 2000) g;
CREATE INDEX gj_j ON gj USING gin (j);
ANALYZE gj;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gj WHERE j ? 'k1';
SELECT count(*) FROM gj WHERE j ? 'k1';
SELECT count(*) FROM gj WHERE j ? 'deep1';
SELECT count(*) FROM gj WHERE j ? 'x';
SELECT count(*) FROM gj WHERE j ?| ARRAY['k1', 'k2'];
SELECT count(*) FROM gj WHERE j ?& ARRAY['k1', 'x'];
SELECT count(*) FROM gj WHERE j ?| ARRAY['k1', NULL];
SELECT count(*) FROM gj WHERE j ?& ARRAY[]::text[];
SELECT count(*) FROM gj WHERE j @> '{"n": {"deep1": true}}';
UPDATE gj SET j = '{"k1": 0}' WHERE id <= 20;
SELECT count(*) FROM gj WHERE j ? 'k1';
-- A partial index: its predicate isn't the structure's.
CREATE TABLE gp (id int, tsv tsvector);
INSERT INTO gp SELECT g, to_tsvector('simple', 'x' || (g % 9) || ' common') FROM generate_series(1, 2000) g;
CREATE INDEX gp_tsv ON gp USING gin (tsv) WHERE id > 1000;
ANALYZE gp;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gp WHERE tsv @@ to_tsquery('simple', 'x3 & common') AND id > 1000;
SELECT count(*) FROM gp WHERE tsv @@ to_tsquery('simple', 'x3 & common') AND id > 1000;
RESET enable_seqscan;
DROP TABLE ge, gj, gp;
