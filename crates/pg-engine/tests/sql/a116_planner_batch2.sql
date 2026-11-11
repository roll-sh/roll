-- Round 23, agent 116, batch 2: planning of job-queue fetches and the planner choices it
-- touched, recorded from PostgreSQL 18.6.
CREATE TABLE a116_jobs (id bigserial PRIMARY KEY, state text NOT NULL DEFAULT 'available', queue text NOT NULL DEFAULT 'default',
  worker text NOT NULL, args jsonb NOT NULL DEFAULT '{}', attempt int NOT NULL DEFAULT 0, priority int NOT NULL DEFAULT 0,
  scheduled_at timestamp NOT NULL DEFAULT '2026-01-01', attempted_at timestamp);
CREATE INDEX a116_jobs_fetch ON a116_jobs (state, queue, priority, scheduled_at, id);
INSERT INTO a116_jobs (worker, priority, scheduled_at) SELECT 'W', g % 4, timestamp '2026-01-01' - g * interval '1 s' FROM generate_series(1, 5000) g;
VACUUM ANALYZE a116_jobs;
-- Oban's fetch: the IN sub-select (no grouping of its own) unique-ified under a nested loop.
EXPLAIN (COSTS OFF) UPDATE a116_jobs SET state = 'executing', attempted_at = '2026-01-02', attempt = attempt + 1
WHERE id IN (SELECT id FROM a116_jobs WHERE state = 'available' AND queue = 'default'
             ORDER BY priority, scheduled_at, id LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING id;
UPDATE a116_jobs SET state = 'executing', attempted_at = '2026-01-02', attempt = attempt + 1
WHERE id IN (SELECT id FROM a116_jobs WHERE state = 'available' AND queue = 'default'
             ORDER BY priority, scheduled_at, id LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING id;
-- A merge join's start: mergejoinscansel reads the columns' statistics, never the index
-- (get_variable_range).
CREATE TABLE a116_m1 (k int PRIMARY KEY, v int);
CREATE TABLE a116_m2 (k int, w int);
INSERT INTO a116_m1 SELECT g, g FROM generate_series(1, 20000) g;
INSERT INTO a116_m2 SELECT g + 5000, g FROM generate_series(1, 20000) g;
CREATE INDEX ON a116_m2 (k);
ANALYZE a116_m1, a116_m2;
INSERT INTO a116_m2 SELECT g, g FROM generate_series(1, 100) g;
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN SELECT * FROM a116_m1 JOIN a116_m2 USING (k) ORDER BY k LIMIT 5;
RESET enable_hashjoin;
RESET enable_nestloop;
-- A sorted grouping as cheap as the hashed one (fuzzily) removes it from the relation's
-- paths (add_path: its pathkeys are better): a LIMIT reads the sorted one.
EXPLAIN WITH t AS (SELECT queue, sum(attempt) AS v FROM a116_jobs WHERE scheduled_at > '2026-02-01' GROUP BY queue)
SELECT queue, v FROM t LIMIT 10;
DROP TABLE a116_jobs, a116_m1, a116_m2;
