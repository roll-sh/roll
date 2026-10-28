-- IN (SELECT ... LIMIT n FOR UPDATE SKIP LOCKED): a job queue's fetch. The sub-select joins
-- as a subquery of its own ("ANY_subquery"), locking the rows it returns.
CREATE TABLE a88_jobs (id int PRIMARY KEY, queue text NOT NULL, state text NOT NULL DEFAULT 'available',
  priority int NOT NULL DEFAULT 0, attempt int NOT NULL DEFAULT 0);
CREATE INDEX a88_jobs_fetch ON a88_jobs (queue, state, priority, id);
INSERT INTO a88_jobs (id, queue, priority) SELECT g, 'q' || (g % 3), g % 5 FROM generate_series(1, 3000) g;
ANALYZE a88_jobs;
EXPLAIN (COSTS OFF)
UPDATE a88_jobs SET state = 'executing', attempt = attempt + 1
WHERE id IN (SELECT id FROM a88_jobs WHERE state = 'available' AND queue = 'q1'
             ORDER BY priority, id LIMIT 5 FOR UPDATE SKIP LOCKED)
RETURNING id;
UPDATE a88_jobs SET state = 'executing', attempt = attempt + 1
WHERE id IN (SELECT id FROM a88_jobs WHERE state = 'available' AND queue = 'q1'
             ORDER BY priority, id LIMIT 5 FOR UPDATE SKIP LOCKED)
RETURNING id, state, attempt;
EXPLAIN (COSTS OFF)
SELECT id, priority FROM a88_jobs
WHERE id IN (SELECT id FROM a88_jobs WHERE queue = 'q2' ORDER BY priority, id LIMIT 3 FOR UPDATE)
ORDER BY id;
SELECT id, priority FROM a88_jobs
WHERE id IN (SELECT id FROM a88_jobs WHERE queue = 'q2' ORDER BY priority, id LIMIT 3 FOR UPDATE)
ORDER BY id;
EXPLAIN (COSTS OFF)
DELETE FROM a88_jobs
WHERE id IN (SELECT id FROM a88_jobs WHERE state = 'executing' ORDER BY id LIMIT 2 FOR NO KEY UPDATE SKIP LOCKED);
WITH d AS (DELETE FROM a88_jobs
  WHERE id IN (SELECT id FROM a88_jobs WHERE state = 'executing' ORDER BY id LIMIT 2 FOR NO KEY UPDATE SKIP LOCKED)
  RETURNING id)
SELECT id FROM d ORDER BY id;
SELECT state, count(*) FROM a88_jobs GROUP BY state ORDER BY state;
DROP TABLE a88_jobs;
