\set ON_ERROR_STOP on
\pset pager off
SELECT version();
SELECT name,setting,unit,source FROM pg_settings WHERE name IN ('max_parallel_workers','max_worker_processes','max_parallel_workers_per_gather','parallel_setup_cost','parallel_tuple_cost','min_parallel_table_scan_size','min_parallel_index_scan_size','seq_page_cost','random_page_cost','plan_cache_mode','track_counts','track_activities','stats_fetch_consistency','compute_query_id','shared_buffers','maintenance_work_mem','max_connections','statement_timeout') ORDER BY name;
SELECT backend_type,count(*) FROM pg_stat_activity GROUP BY backend_type ORDER BY backend_type;
SELECT current_setting('max_parallel_workers')::int >= 2 AND current_setting('max_worker_processes')::int >= 2 AND NOT EXISTS(SELECT FROM pg_stat_activity WHERE backend_type='parallel worker') AS two_configured_workers_no_active_parallel_workers;
SELECT parallel_workers_to_launch,parallel_workers_launched FROM pg_stat_database WHERE datname=current_database();
