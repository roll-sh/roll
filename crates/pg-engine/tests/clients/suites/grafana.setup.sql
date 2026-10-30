-- Grafana's own database (grafana_app: its migrations make ~100 tables) and the data a
-- PostgreSQL datasource reads (grafana_ds): a metrics table with time series of three hosts,
-- an events table for annotations, a view, an enum and a few types the query editor lists.
DROP DATABASE IF EXISTS grafana_app;
DROP DATABASE IF EXISTS grafana_ds;
CREATE DATABASE grafana_app;
CREATE DATABASE grafana_ds;
\c grafana_ds
CREATE TYPE level AS ENUM ('info', 'warn', 'error');
CREATE TABLE metrics (
  ts timestamptz NOT NULL,
  host text NOT NULL,
  cpu double precision,
  mem bigint,
  load numeric(6, 2),
  PRIMARY KEY (host, ts)
);
INSERT INTO metrics
SELECT '2026-01-01 00:00:00+00'::timestamptz + make_interval(mins => i), h,
       round(((i * 7 + length(h) * 13) % 100)::numeric / 3, 3)::float8,
       1000000 + i * 1024 * length(h),
       ((i % 17) * 1.25)::numeric(6, 2)
FROM generate_series(0, 359) i, unnest(ARRAY['web-1', 'web-2', 'db-1']) h;
CREATE INDEX metrics_ts ON metrics (ts);
CREATE TABLE events (
  id serial PRIMARY KEY,
  at timestamptz NOT NULL,
  ends timestamptz,
  title text NOT NULL,
  tags text[],
  lvl level NOT NULL DEFAULT 'info'
);
INSERT INTO events (at, ends, title, tags, lvl) VALUES
  ('2026-01-01 01:00+00', NULL, 'deploy v1', '{deploy,web}', 'info'),
  ('2026-01-01 02:30+00', '2026-01-01 02:45+00', 'outage', '{incident}', 'error'),
  ('2026-01-01 04:00+00', NULL, 'scale up', '{ops,web}', 'warn');
CREATE VIEW host_avg AS SELECT host, avg(cpu) AS cpu, max(mem) AS mem FROM metrics GROUP BY host;
CREATE SCHEMA reports;
CREATE TABLE reports.daily (day date PRIMARY KEY, total numeric, note text);
INSERT INTO reports.daily VALUES ('2026-01-01', 12.5, 'first'), ('2026-01-02', NULL, NULL);
COMMENT ON TABLE metrics IS 'per-minute host metrics';
