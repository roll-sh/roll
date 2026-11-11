-- pg_dump at scale: 10 schemas x 300 tables (each a bigserial key, an index, a few rows)
-- and one big table of BIG_ROWS rows. Run with psql -v big=<rows>.
SET client_min_messages = warning;
SELECT format('CREATE SCHEMA s%s', s) FROM generate_series(1, 10) s \gexec
SELECT format('CREATE TABLE s%s.t%s (id bigserial PRIMARY KEY, a int NOT NULL, b text, c timestamptz DEFAULT now(), CHECK (a >= 0))', s, t)
  FROM generate_series(1, 10) s, generate_series(1, 300) t \gexec
SELECT format('CREATE INDEX ON s%s.t%s (a)', s, t) FROM generate_series(1, 10) s, generate_series(1, 300) t \gexec
SELECT format('COMMENT ON TABLE s%s.t%s IS %L', s, t, 'table ' || t) FROM generate_series(1, 10) s, generate_series(1, 300) t \gexec
SELECT format('CREATE VIEW s%s.v%s AS SELECT id, a FROM s%s.t%s WHERE a > 5', s, t, s, t) FROM generate_series(1, 10) s, generate_series(1, 100) t \gexec
SELECT format('INSERT INTO s%s.t%s (a, b) SELECT g, md5(g::text) FROM generate_series(1, 20) g', s, t)
  FROM generate_series(1, 10) s, generate_series(1, 300) t \gexec
CREATE TABLE public.big (id bigint PRIMARY KEY, a int, b text, c timestamptz);
SELECT format($$INSERT INTO public.big SELECT g, g %% 1000, repeat(md5(g::text), 2), %L::timestamptz + g * interval %L FROM generate_series(%s, %s) g$$, $$2026-01-01$$, $$1 second$$, k * (:big / 10) + 1, (k + 1) * (:big / 10)) FROM generate_series(0, 9) k \gexec
CREATE INDEX big_a ON public.big (a);
ANALYZE;
