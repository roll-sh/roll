-- Concurrent build formation errors leave a not-ready INVALID index, including retries.
SET max_parallel_maintenance_workers = 0;
CREATE FUNCTION a163_cic_text(n int) RETURNS text LANGUAGE sql IMMUTABLE
AS $$ SELECT left(string_agg(md5(g::text), ''), n) FROM generate_series(1, (n+31)/32) g $$;
CREATE TABLE a163_cic (k int, payload text);
INSERT INTO a163_cic VALUES (1, a163_cic_text(8300));
CREATE INDEX CONCURRENTLY a163_cic_i ON a163_cic(k) INCLUDE(payload);
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
-- Retrying an invalid index also leaves its failed replacement behind.
REINDEX INDEX CONCURRENTLY a163_cic_i;
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
UPDATE a163_cic SET payload='small';
REINDEX INDEX CONCURRENTLY a163_cic_i;
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
DROP TABLE a163_cic;
-- Formation wins over an earlier duplicate, then uniqueness fails after removing it.
CREATE TABLE a163_cic (k int, payload text);
INSERT INTO a163_cic VALUES (1,'first'), (1,'second'), (2,a163_cic_text(8300));
CREATE UNIQUE INDEX CONCURRENTLY a163_cic_u ON a163_cic(k) INCLUDE(payload);
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
REINDEX INDEX CONCURRENTLY a163_cic_u;
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
DELETE FROM a163_cic WHERE k=2;
REINDEX INDEX CONCURRENTLY a163_cic_u;
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
DELETE FROM a163_cic WHERE payload='second';
REINDEX INDEX CONCURRENTLY a163_cic_u;
SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='a163_cic'::regclass ORDER BY c.relname;
DROP TABLE a163_cic;
DROP FUNCTION a163_cic_text;
RESET max_parallel_maintenance_workers;
