\set ON_ERROR_STOP on
\set VERBOSITY verbose
\pset pager off
\pset format unaligned
\pset tuples_only on
SET client_encoding='UTF8';
SET statement_timeout='60s';
SET max_parallel_maintenance_workers=0;
SET max_parallel_workers=1024;
SET min_parallel_table_scan_size=0;
SET min_parallel_index_scan_size=0;
SET maintenance_work_mem='128MB';
SELECT json_build_object('tag','settings','server_encoding',current_setting('server_encoding'),'client_encoding',current_setting('client_encoding'),'max_parallel_maintenance_workers',current_setting('max_parallel_maintenance_workers'),'max_parallel_workers',current_setting('max_parallel_workers'),'max_worker_processes',current_setting('max_worker_processes'),'min_parallel_table_scan_size',current_setting('min_parallel_table_scan_size'),'min_parallel_index_scan_size',current_setting('min_parallel_index_scan_size'),'maintenance_work_mem',current_setting('maintenance_work_mem'));
CREATE TABLE ce(k int, p0 text, p1 text, p2 text, p3 text, p4 text, p5 text, p6 text, p7 text, p8 text, p9 text, p10 text, p11 text, p12 text, p13 text, p14 text, p15 text) WITH (parallel_workers=2);
ALTER TABLE ce ALTER COLUMN p0 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p1 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p2 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p3 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p4 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p5 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p6 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p7 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p8 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p9 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p10 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p11 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p12 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p13 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p14 SET STORAGE PLAIN;
ALTER TABLE ce ALTER COLUMN p15 SET STORAGE PLAIN;
INSERT INTO ce SELECT g, repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120), repeat('é', 120) FROM generate_series(1,6000) g;
SELECT json_build_object('tag','encoding_prebuild','server_encoding',current_setting('server_encoding'),'client_encoding',current_setting('client_encoding'),'octet_length',octet_length(p0),'length',length(p0),'ctid',ctid::text,'key',k) FROM ce WHERE k=1;
SELECT json_build_object('tag','column_storage','column',attname,'storage',attstorage::text) FROM pg_attribute WHERE attrelid='ce'::regclass AND attnum>1 AND NOT attisdropped ORDER BY attnum;
SELECT json_build_object('tag','prebuild_count','count',count(*)) FROM ce;
\echo BEGIN_RESULT encoding_latin1_w0
\set ON_ERROR_STOP off
CREATE INDEX ce_i ON ce(k) INCLUDE(p0, p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15);
\echo RESULT encoding_latin1_w0 :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT json_build_object('tag','postbuild','count',count(*),'index_exists',to_regclass('ce_i') IS NOT NULL) FROM ce;
