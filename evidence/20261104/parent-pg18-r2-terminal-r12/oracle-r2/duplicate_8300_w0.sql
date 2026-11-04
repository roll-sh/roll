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
CREATE FUNCTION ci_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$
SELECT left(string_agg(md5(g::text), '' ORDER BY g), n)
FROM generate_series(1, (n+31)/32) g $$;
CREATE TABLE ci(k int, payload text) WITH (parallel_workers=2);
INSERT INTO ci SELECT CASE WHEN g=2 THEN 1 ELSE g END, CASE WHEN g=6000 THEN ci_text(8300) ELSE 'small' END FROM generate_series(1,6000) g;
SELECT json_build_object('tag','prebuild_rows','ctid',ctid::text,'key',k,'actual_payload_bytes',octet_length(payload),'payload_characters',length(payload),'heap_stored_payload_bytes',pg_column_size(payload)) FROM ci WHERE octet_length(payload)>5 OR k IN (1,2) ORDER BY k;
SELECT json_build_object('tag','prebuild_count','count',count(*)) FROM ci;
\echo BEGIN_RESULT duplicate_8300_w0
\set ON_ERROR_STOP off
CREATE UNIQUE INDEX ci_i ON ci(k) INCLUDE(payload);
\echo RESULT duplicate_8300_w0 :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT json_build_object('tag','postbuild','count',count(*),'index_exists',to_regclass('ci_i') IS NOT NULL) FROM ci;
