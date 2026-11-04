-- CACHE-ONLY / NOT RUN. psql18 extended-protocol control, not SQL PREPARE.
-- Capture actual PG18.6 identity and engine source before running after pinned terminal.
CREATE TABLE p171_parallel AS SELECT g AS a FROM generate_series(1,3000000) g;
ANALYZE p171_parallel;
SET max_parallel_workers_per_gather=0;
SELECT a FROM p171_parallel WHERE a % 3000000=0 \parse p171_workers
SET max_parallel_workers_per_gather=2;
SELECT pg_stat_force_next_flush();
SELECT parallel_workers_to_launch AS before_plan, parallel_workers_launched AS before_launch
FROM pg_stat_database WHERE datname=current_database() \gset
\bind_named p171_workers
\g
SELECT pg_stat_force_next_flush();
SELECT parallel_workers_to_launch-:before_plan AS planned_delta,
       parallel_workers_launched-:before_launch AS launched_delta
FROM pg_stat_database WHERE datname=current_database();
EXPLAIN SELECT a FROM p171_parallel WHERE a % 3000000=0;

SET parallel_setup_cost=1000000000;
SELECT a FROM p171_parallel WHERE a % 3000000=0 \parse p171_setup
RESET parallel_setup_cost;
SELECT pg_stat_force_next_flush();
SELECT parallel_workers_to_launch AS before_plan, parallel_workers_launched AS before_launch
FROM pg_stat_database WHERE datname=current_database() \gset
\bind_named p171_setup
\g
SELECT pg_stat_force_next_flush();
SELECT parallel_workers_to_launch-:before_plan AS planned_delta,
       parallel_workers_launched-:before_launch AS launched_delta
FROM pg_stat_database WHERE datname=current_database();
EXPLAIN SELECT a FROM p171_parallel WHERE a % 3000000=0;
