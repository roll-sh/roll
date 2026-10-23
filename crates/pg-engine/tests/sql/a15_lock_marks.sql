-- Round 16, agent 15: a locking level's FROM subquery marked by its whole row (`s.*`) keeps
-- every output (remove_unused_subquery_outputs keeps them all for a whole-row Var), and the
-- Limits over a level's LockRows return its row marks too (their input's target list).
CREATE TABLE a15_la (id int PRIMARY KEY, x int, other text);
CREATE TABLE a15_lb (id int, y int);
INSERT INTO a15_la SELECT g, g % 10, 'o' || g FROM generate_series(1, 100) g;
INSERT INTO a15_lb SELECT g % 50, g FROM generate_series(1, 100) g;
ANALYZE a15_la;
ANALYZE a15_lb;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT s.id FROM (SELECT * FROM a15_la OFFSET 0) s, a15_lb WHERE s.id = a15_lb.id FOR SHARE OF a15_lb;
SELECT s.id FROM (SELECT * FROM a15_la OFFSET 0) s, a15_lb WHERE s.id = a15_lb.id AND s.id < 3
  ORDER BY 1 FOR SHARE OF a15_lb;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT s.id, b.y FROM (SELECT id, x, other FROM a15_la WHERE id < 5 FOR SHARE) s JOIN a15_lb b ON s.id = b.id FOR UPDATE OF b;
SELECT s.id, b.y FROM (SELECT id, x, other FROM a15_la WHERE id < 3 FOR SHARE) s JOIN a15_lb b ON s.id = b.id
  ORDER BY 1, 2 FOR UPDATE OF b;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a15_la LIMIT 5 FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM a15_la LIMIT 5 FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM a15_la ORDER BY id LIMIT 2 OFFSET 1 FOR SHARE;
SELECT x FROM a15_la ORDER BY id LIMIT 2 OFFSET 1 FOR SHARE;
-- A level whose lock went into a FROM subquery planned apart (which locks its own rows)
-- still has a LockRows, marking the subquery's whole row.
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT * FROM a15_la OFFSET 0) s FOR UPDATE OF s;
SELECT * FROM (SELECT * FROM a15_la OFFSET 0) s WHERE id < 3 ORDER BY 1 FOR UPDATE OF s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT * FROM a15_la OFFSET 0) s FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT * FROM a15_la FOR UPDATE) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT * FROM a15_la FOR UPDATE) s FOR UPDATE OF s;
DROP TABLE a15_la, a15_lb;
