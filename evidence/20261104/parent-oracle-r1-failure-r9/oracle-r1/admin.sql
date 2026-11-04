\set ON_ERROR_STOP on
\set VERBOSITY verbose
\pset pager off
\pset format unaligned
\pset tuples_only on
SET client_encoding='UTF8';
SET statement_timeout='60s';
SELECT version();
SHOW server_version_num;
SELECT name,setting,unit,source FROM pg_settings WHERE name IN ('shared_buffers','maintenance_work_mem','max_parallel_workers','max_parallel_maintenance_workers','max_worker_processes','autovacuum','data_directory','lc_collate','lc_ctype','log_min_messages','max_connections');
CREATE DATABASE minimum_asc_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE minimum_desc_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE include_2800_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE include_8300_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE duplicate_2800_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE duplicate_8300_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE duplicate_0_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE minimum_asc_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE minimum_desc_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE include_2800_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE include_8300_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE duplicate_2800_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE duplicate_8300_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE duplicate_0_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE stats_2800 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE stats_8300 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE encoding_latin1_w0 TEMPLATE template0 ENCODING 'LATIN1' LOCALE 'C';
CREATE DATABASE encoding_latin1_w2 TEMPLATE template0 ENCODING 'LATIN1' LOCALE 'C';
CREATE DATABASE encoding_utf8_w0 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
CREATE DATABASE encoding_utf8_w2 TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';
