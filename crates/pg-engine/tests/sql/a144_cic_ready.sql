-- pg_index flags after failed concurrent builds (agent 144; agent 156's difftest finding): a
-- CREATE UNIQUE INDEX CONCURRENTLY failing in its build leaves the index INVALID and not
-- ready; REINDEX CONCURRENTLY failing likewise leaves its _ccnew copy; a REINDEX that
-- succeeds makes the index valid and ready again.
CREATE TABLE a144_cic (id int, k int);
INSERT INTO a144_cic SELECT g, g % 50 FROM generate_series(1, 200) g;
CREATE UNIQUE INDEX CONCURRENTLY a144_cic_k ON a144_cic (k);
SELECT c.relname, i.indisvalid, i.indisready, i.indislive
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'a144_cic'::regclass ORDER BY 1;
INSERT INTO a144_cic VALUES (201, 1);
SELECT count(*) FROM a144_cic;
DELETE FROM a144_cic WHERE id > 50;
REINDEX INDEX a144_cic_k;
SELECT c.relname, i.indisvalid, i.indisready, i.indislive
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'a144_cic'::regclass ORDER BY 1;
INSERT INTO a144_cic VALUES (300, 1);
INSERT INTO a144_cic VALUES (301, 51);
CREATE INDEX CONCURRENTLY a144_cic_id ON a144_cic (id);
CREATE INDEX a144_cic_both ON a144_cic (k, id);
SELECT c.relname, i.indisvalid, i.indisready, i.indislive
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'a144_cic'::regclass ORDER BY 1;
DROP TABLE a144_cic;
-- REINDEX CONCURRENTLY of a unique index whose rows now repeat its key.
CREATE TABLE a144_cic2 (k int);
INSERT INTO a144_cic2 SELECT g FROM generate_series(1, 20) g;
CREATE UNIQUE INDEX a144_cic2_k ON a144_cic2 (k);
ALTER INDEX a144_cic2_k ALTER COLUMN 1 SET STATISTICS 100;
DROP INDEX a144_cic2_k;
CREATE INDEX a144_cic2_plain ON a144_cic2 (k);
INSERT INTO a144_cic2 VALUES (1);
CREATE UNIQUE INDEX CONCURRENTLY a144_cic2_u ON a144_cic2 (k);
SELECT c.relname, i.indisvalid, i.indisready
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'a144_cic2'::regclass ORDER BY 1;
DELETE FROM a144_cic2 WHERE ctid = (SELECT max(ctid) FROM a144_cic2 WHERE k = 1);
REINDEX INDEX CONCURRENTLY a144_cic2_u;
SELECT c.relname, i.indisvalid, i.indisready
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'a144_cic2'::regclass ORDER BY 1;
DROP TABLE a144_cic2;
