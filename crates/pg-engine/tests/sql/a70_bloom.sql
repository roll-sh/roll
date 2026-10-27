-- bloom on the native index access method interface (agent 70): sizes, costs and page reuse as
-- bloom's own blbuild / blinsert / blvacuum lay pages out.
CREATE EXTENSION bloom;
CREATE TABLE a70_tb (i int4, t text) WITH (autovacuum_enabled = off);
INSERT INTO a70_tb SELECT i % 13, i::text FROM generate_series(1, 5000) i;
CREATE INDEX a70_tb_b ON a70_tb USING bloom (i, t) WITH (length = 64, col1 = 3);
SELECT pg_relation_size('a70_tb_b') / 8192, relpages, reltuples FROM pg_class WHERE relname = 'a70_tb_b';
INSERT INTO a70_tb SELECT i % 13, i::text FROM generate_series(1, 700) i;
SELECT pg_relation_size('a70_tb_b') / 8192;
EXPLAIN SELECT * FROM a70_tb WHERE i = 3 AND t = '42';
DELETE FROM a70_tb WHERE i < 6;
VACUUM a70_tb;
SELECT pg_relation_size('a70_tb_b') / 8192, relpages, reltuples FROM pg_class WHERE relname = 'a70_tb_b';
INSERT INTO a70_tb SELECT i % 13, i::text FROM generate_series(1, 3000) i;
SELECT pg_relation_size('a70_tb_b') / 8192;
DELETE FROM a70_tb;
VACUUM a70_tb;
INSERT INTO a70_tb SELECT i % 13, i::text FROM generate_series(1, 2000) i;
SELECT pg_relation_size('a70_tb_b') / 8192, relpages FROM pg_class WHERE relname = 'a70_tb_b';
ANALYZE a70_tb;
SET enable_seqscan = off;
EXPLAIN SELECT * FROM a70_tb WHERE i = 3;
SELECT count(*) FROM a70_tb WHERE i = 3 AND t = '42';
ALTER INDEX a70_tb_b SET (length = 4097);
ALTER INDEX a70_tb_b SET (col33 = 2);
CREATE INDEX ON a70_tb USING bloom (i) WITH (length = 0);
DROP TABLE a70_tb;
DROP EXTENSION bloom;
