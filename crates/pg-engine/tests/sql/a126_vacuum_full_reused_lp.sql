-- A deleted row's line pointer, freed by VACUUM and taken by a new row, isn't found
-- under the deleted row's key too: VACUUM FULL copies each live row once.
CREATE EXTENSION IF NOT EXISTS pgstattuple;
CREATE TABLE a126_vf (id int PRIMARY KEY, a int, t text);
CREATE INDEX a126_vf_a ON a126_vf (a, id);
INSERT INTO a126_vf SELECT g, g % 17, repeat('t', g % 50) FROM generate_series(1, 4000) g;
DELETE FROM a126_vf WHERE id BETWEEN 1000 AND 1600 AND id % 4 <> 0;
UPDATE a126_vf SET t = t || 'u' WHERE id % 13 = 0;
UPDATE a126_vf SET a = a + 100 WHERE id % 31 = 0;
UPDATE a126_vf SET t = 'hot' WHERE id % 37 = 0;
DELETE FROM a126_vf WHERE id % 41 = 0;
VACUUM a126_vf;
INSERT INTO a126_vf SELECT g, 1, 'again' FROM generate_series(6001, 6200) g;
SELECT count(*) FROM a126_vf;
SELECT tuple_count, tuple_len, dead_tuple_count FROM pgstattuple('a126_vf');
VACUUM FULL a126_vf;
SELECT tuple_count, tuple_len, dead_tuple_count, free_space FROM pgstattuple('a126_vf');
SELECT pg_relation_size('a126_vf') / 8192 AS pages;
SELECT count(*), count(DISTINCT ctid) FROM a126_vf;
-- A row deleted after a HOT update: pruning frees its last version's line pointer.
CREATE TABLE a126_vf2 (id int PRIMARY KEY, v int);
INSERT INTO a126_vf2 SELECT g, 0 FROM generate_series(1, 200) g;
UPDATE a126_vf2 SET v = 1 WHERE id % 2 = 0;
DELETE FROM a126_vf2 WHERE id % 2 = 0;
VACUUM a126_vf2;
INSERT INTO a126_vf2 SELECT g, 2 FROM generate_series(1001, 1100) g;
VACUUM FULL a126_vf2;
SELECT tuple_count, dead_tuple_count FROM pgstattuple('a126_vf2');
SELECT count(*), count(DISTINCT ctid) FROM a126_vf2;
DROP TABLE a126_vf, a126_vf2;
