-- pgvector's ivfflat and hnsw (agent 70): DDL, plans, results, maintenance. Small tables, where
-- the probes and ef_search reach every row, so results don't depend on the builds' random
-- choices (ivfflat's costs don't either; hnsw's sizes do, so its plans show no costs).
CREATE EXTENSION vector;
SET max_parallel_maintenance_workers = 0;
CREATE TABLE a70_items (id int, v vector(3), h halfvec(3), b bit(8));
INSERT INTO a70_items SELECT i, ARRAY[i % 7, i % 11, i % 13]::vector,
  ARRAY[i % 5, i % 3, 1]::halfvec, (i % 256)::bit(8) FROM generate_series(1, 300) i;
INSERT INTO a70_items (id) VALUES (301);
ANALYZE a70_items;
-- Errors and notices.
CREATE TABLE a70_nodim (v vector);
CREATE INDEX ON a70_nodim USING ivfflat (v);
CREATE INDEX ON a70_nodim USING hnsw (v);
CREATE INDEX ON a70_items USING ivfflat (v) WITH (lists = 0);
CREATE INDEX ON a70_items USING hnsw (v) WITH (m = 1);
CREATE INDEX ON a70_items USING hnsw (v) WITH (m = 40, ef_construction = 50);
CREATE INDEX ON a70_items USING ivfflat (v) WITH (foo = 1);
CREATE UNIQUE INDEX ON a70_items USING hnsw (v);
CREATE INDEX ON a70_items USING hnsw (v, h);
CREATE INDEX a70_ivf ON a70_items USING ivfflat (v vector_l2_ops) WITH (lists = 4);
SELECT indexdef FROM pg_indexes WHERE indexname = 'a70_ivf';
SELECT relpages, reltuples FROM pg_class WHERE relname = 'a70_ivf';
SELECT pg_relation_size('a70_ivf') / 8192 AS pages;
SET ivfflat.probes = 4;
EXPLAIN SELECT id, v <-> '[3,1,2]' AS d FROM a70_items ORDER BY 2 LIMIT 5;
SELECT id, v <-> '[3,1,2]' AS d FROM a70_items ORDER BY v <-> '[3,1,2]', id LIMIT 5;
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <-> '[3,1,2]') s;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items WHERE id > 100 ORDER BY v <-> '[3,1,2]' LIMIT 5;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items ORDER BY v <-> (SELECT v FROM a70_items WHERE id = 7) LIMIT 3;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items ORDER BY v <=> '[3,1,2]' LIMIT 5;
SET ivfflat.iterative_scan = relaxed_order;
SELECT id FROM a70_items WHERE id % 50 = 0 ORDER BY v <-> '[3,1,2]', id LIMIT 3;
RESET ivfflat.iterative_scan;
-- Maintenance: updates, deletes, VACUUM, REINDEX, TRUNCATE rolled back.
UPDATE a70_items SET v = '[100,100,100]' WHERE id = 1;
SELECT id FROM a70_items ORDER BY v <-> '[100,100,100]' LIMIT 2;
DELETE FROM a70_items WHERE id <= 100;
VACUUM a70_items;
SELECT reltuples FROM pg_class WHERE relname = 'a70_ivf';
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <-> '[3,1,2]') s;
REINDEX INDEX a70_ivf;
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <-> '[3,1,2]') s;
BEGIN;
TRUNCATE a70_items;
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <-> '[3,1,2]') s;
ROLLBACK;
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <-> '[3,1,2]') s;
ALTER INDEX a70_ivf SET (lists = 2);
SELECT reloptions FROM pg_class WHERE relname = 'a70_ivf';
DROP INDEX a70_ivf;
-- hnsw.
CREATE INDEX a70_hnsw ON a70_items USING hnsw (v vector_cosine_ops) WITH (m = 8, ef_construction = 32);
CREATE INDEX a70_hnsw_h ON a70_items USING hnsw (h halfvec_l2_ops);
CREATE INDEX a70_hnsw_b ON a70_items USING hnsw (b bit_hamming_ops);
SET hnsw.ef_search = 400;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items ORDER BY v <=> '[3,1,2]' LIMIT 5;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items ORDER BY h <-> '[1,1,1]' LIMIT 5;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items ORDER BY b <~> '10101010' LIMIT 5;
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <=> '[3,1,2]') s;
SELECT count(*) FROM (SELECT id FROM a70_items ORDER BY v <=> '[0,0,0]') s;
SET hnsw.iterative_scan = strict_order;
SELECT count(*) FROM (SELECT id FROM a70_items WHERE id % 3 = 0 ORDER BY h <-> '[1,1,1]' LIMIT 20) s;
RESET hnsw.iterative_scan;
EXPLAIN (COSTS OFF) SELECT id FROM a70_items ORDER BY v <-> '[3,1,2]' LIMIT 5;
\d a70_items
DROP TABLE a70_items, a70_nodim;
DROP EXTENSION vector;
