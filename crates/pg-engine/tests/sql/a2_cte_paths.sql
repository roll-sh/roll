-- A materialized CTE's order offered to the query above (a merge join over its scan, no
-- sort for an ORDER BY it already has); a recursive union's estimates (cost_recursive_union:
-- about ten iterations of the recursive term) and its worktable's (ten times the
-- non-recursive term, no statistics through the self-reference), which decide the
-- recursive term's join and so its rows' order.
CREATE TABLE a2_ct (unique1 int, unique2 int);
INSERT INTO a2_ct SELECT g, g FROM generate_series(0, 9999) g;
CREATE INDEX ON a2_ct (unique1);
VACUUM ANALYZE a2_ct;
EXPLAIN (COSTS OFF)
WITH x AS MATERIALIZED (SELECT unique1 FROM a2_ct b ORDER BY unique1)
SELECT count(*) FROM a2_ct a WHERE unique1 IN (SELECT * FROM x);
WITH x AS MATERIALIZED (SELECT unique1 FROM a2_ct b ORDER BY unique1)
SELECT count(*) FROM a2_ct a WHERE unique1 IN (SELECT * FROM x);
EXPLAIN (COSTS OFF)
WITH x AS MATERIALIZED (SELECT unique1, unique2 FROM a2_ct b WHERE unique1 < 100 ORDER BY unique2)
SELECT * FROM x ORDER BY unique2 LIMIT 3;
DROP TABLE a2_ct;

CREATE TEMP TABLE a2_graph (f int, t int, label text);
INSERT INTO a2_graph VALUES
  (1, 2, 'arc 1 -> 2'), (1, 3, 'arc 1 -> 3'), (2, 3, 'arc 2 -> 3'),
  (1, 4, 'arc 1 -> 4'), (4, 5, 'arc 4 -> 5'), (5, 1, 'arc 5 -> 1');
EXPLAIN (COSTS OFF)
WITH RECURSIVE search_graph(f, t, label, is_cycle, path) AS (
  SELECT *, false, array[row(g.f, g.t)] FROM a2_graph g
  UNION ALL
  SELECT g.*, row(g.f, g.t) = any(path), path || row(g.f, g.t)
  FROM a2_graph g, search_graph sg
  WHERE g.f = sg.t AND NOT is_cycle
)
SELECT f, t, label, is_cycle FROM search_graph;
WITH RECURSIVE search_graph(f, t, label, is_cycle, path) AS (
  SELECT *, false, array[row(g.f, g.t)] FROM a2_graph g
  UNION ALL
  SELECT g.*, row(g.f, g.t) = any(path), path || row(g.f, g.t)
  FROM a2_graph g, search_graph sg
  WHERE g.f = sg.t AND NOT is_cycle
)
SELECT * FROM search_graph;
-- (An earlier item's estimate for a later one's plan; CYCLE's check on the work table's scan.)
EXPLAIN (COSTS OFF)
WITH RECURSIVE
graph(f, t, label) AS (
  VALUES (1, 2, 'arc 1 -> 2'), (1, 3, 'arc 1 -> 3'), (2, 3, 'arc 2 -> 3'),
         (1, 4, 'arc 1 -> 4'), (4, 5, 'arc 4 -> 5'), (5, 1, 'arc 5 -> 1')),
search_graph(f, t, label) AS (
  SELECT * FROM graph g
  UNION ALL
  SELECT g.* FROM graph g, search_graph sg WHERE g.f = sg.t
) CYCLE f, t SET is_cycle TO true DEFAULT false USING path
SELECT f, t, label FROM search_graph;
WITH RECURSIVE
graph(f, t, label) AS (
  VALUES (1, 2, 'arc 1 -> 2'), (1, 3, 'arc 1 -> 3'), (2, 3, 'arc 2 -> 3'),
         (1, 4, 'arc 1 -> 4'), (4, 5, 'arc 4 -> 5'), (5, 1, 'arc 5 -> 1')),
search_graph(f, t, label) AS (
  SELECT * FROM graph g
  UNION ALL
  SELECT g.* FROM graph g, search_graph sg WHERE g.f = sg.t
) CYCLE f, t SET is_cycle TO true DEFAULT false USING path
SELECT f, t, label FROM search_graph;

-- (A CTE or WorkTable Scan sorted for a merge join returns the columns used above, then
-- the keys'.)
CREATE TEMP TABLE a2_graph0 (f int, t int, label text);
INSERT INTO a2_graph0 VALUES (1, 2, 'arc 1 -> 2'), (1, 3, 'arc 1 -> 3'), (2, 3, 'arc 2 -> 3'),
  (1, 4, 'arc 1 -> 4'), (4, 5, 'arc 4 -> 5');
EXPLAIN (VERBOSE, COSTS OFF)
WITH RECURSIVE search_graph(f, t, label) AS (
  SELECT * FROM a2_graph0 g
  UNION ALL
  SELECT g.* FROM a2_graph0 g, search_graph sg WHERE g.f = sg.t
) SEARCH DEPTH FIRST BY f, t SET seq
SELECT * FROM search_graph ORDER BY seq;

-- (A FROM subquery's output nothing reads is a NULL in its target list, also when the
-- subquery has a WITH.)
CREATE TEMP TABLE a2_i8 (q1 bigint, q2 bigint);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT q1 FROM
(
  WITH t_cte AS MATERIALIZED (SELECT * FROM a2_i8 t)
  SELECT q1, (SELECT q2 FROM t_cte WHERE t_cte.q1 = i8.q1) AS t_sub
  FROM a2_i8 i8
) ss;
