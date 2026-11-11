-- Schema for plans/apps.sql (tools/bench/plancpu.py): an application's 200 tables in two
-- schemas with keys, foreign keys, indexes and comments (what ORM and driver metadata queries
-- read), the job-queue tables of tools/bench/access.rs at 10,000 rows, and pgbench's tables
-- (made by `pgbench -i -s 10` before this file).
DO $$
BEGIN
  FOR i IN 1..200 LOOP
    EXECUTE format('CREATE TABLE IF NOT EXISTS %I.t%s (id bigserial PRIMARY KEY, name text NOT NULL, '
      'created_at timestamptz NOT NULL DEFAULT now(), parent_id bigint %s, c5 int, amount numeric(12,2), '
      'status char(1) NOT NULL DEFAULT ''a'')',
      CASE WHEN i % 4 = 0 THEN 'app2' ELSE 'public' END, i,
      CASE WHEN i > 1 AND i % 4 <> 0 THEN format('REFERENCES %I.t%s (id)', CASE WHEN (i - 1) % 4 = 0 THEN 'app2' ELSE 'public' END, i - 1) ELSE '' END);
    EXECUTE format('CREATE INDEX IF NOT EXISTS t%s_name ON %I.t%s (name)', i, CASE WHEN i % 4 = 0 THEN 'app2' ELSE 'public' END, i);
    EXECUTE format('COMMENT ON TABLE %I.t%s IS %L', CASE WHEN i % 4 = 0 THEN 'app2' ELSE 'public' END, i, 'table ' || i);
  END LOOP;
END $$;
CREATE TABLE IF NOT EXISTS oban_jobs (id bigserial PRIMARY KEY, state text NOT NULL DEFAULT 'available',
  queue text NOT NULL DEFAULT 'default', worker text NOT NULL, args jsonb NOT NULL DEFAULT '{}',
  attempt int NOT NULL DEFAULT 0, priority int NOT NULL DEFAULT 0, scheduled_at timestamp NOT NULL DEFAULT now(),
  attempted_at timestamp);
CREATE INDEX IF NOT EXISTS oban_jobs_fetch ON oban_jobs (state, queue, priority, scheduled_at, id);
INSERT INTO oban_jobs (worker, priority, scheduled_at)
  SELECT 'W', g % 4, now() - g * interval '1 s' FROM generate_series(1, 10000) g;
CREATE TABLE IF NOT EXISTS gw_jobs (id bigserial PRIMARY KEY, task text NOT NULL, priority int NOT NULL DEFAULT 0,
  run_at timestamptz NOT NULL DEFAULT now(), attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 25,
  locked_at timestamptz, locked_by text);
CREATE INDEX IF NOT EXISTS gw_jobs_main ON gw_jobs (priority, run_at, id) WHERE locked_at IS NULL;
INSERT INTO gw_jobs (task, priority, run_at) SELECT 't', g % 3, now() - g * interval '1 s' FROM generate_series(1, 10000) g;
CREATE TABLE IF NOT EXISTS que_jobs (priority smallint NOT NULL DEFAULT 100, run_at timestamptz NOT NULL DEFAULT now(),
  id bigserial PRIMARY KEY, job_class text NOT NULL, error_count int NOT NULL DEFAULT 0, last_error text,
  queue text NOT NULL DEFAULT 'default', args jsonb NOT NULL DEFAULT '[]', finished_at timestamptz, expired_at timestamptz);
CREATE INDEX IF NOT EXISTS que_poll_idx ON que_jobs (queue, priority, run_at, id) WHERE finished_at IS NULL AND expired_at IS NULL;
INSERT INTO que_jobs (priority, run_at, job_class, args)
  SELECT g % 10, now() - g * interval '1 s', 'Job', jsonb_build_array(g) FROM generate_series(1, 10000) g;
VACUUM ANALYZE;
