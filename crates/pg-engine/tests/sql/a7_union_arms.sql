-- A query level's own UNION ALL whose arms stay subqueries (their InitPlans at their
-- tops): each arm's partial plan pays for its InitPlans, and the Parallel Append runs arms
-- costing the same in their range table entries' order, the leftmost arm's copy last
-- (flatten_simple_union_all); setrefs then names their relations in that order. A FROM
-- subquery's UNION ALL keeps its arms in order; a serial Append too.
CREATE TABLE a7_ua (a int) PARTITION BY LIST (a);
CREATE TABLE a7_ua_12 PARTITION OF a7_ua FOR VALUES IN (1, 2) PARTITION BY LIST (a);
CREATE TABLE a7_ua_12_1 PARTITION OF a7_ua_12 FOR VALUES IN (1);
CREATE TABLE a7_ua_12_2 PARTITION OF a7_ua_12 FOR VALUES IN (2);
ALTER TABLE a7_ua_12_1 SET (parallel_workers = 0);
INSERT INTO a7_ua VALUES (1), (2), (2);
BEGIN;
SET LOCAL parallel_setup_cost = 0;
SET LOCAL parallel_tuple_cost = 0;
SET LOCAL max_parallel_workers_per_gather = 2;
SELECT * FROM a7_ua WHERE a = (SELECT 1)
  UNION ALL
SELECT * FROM a7_ua WHERE a = (SELECT 2)
ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT * FROM a7_ua WHERE a = (SELECT 1)
  UNION ALL
SELECT * FROM a7_ua WHERE a = (SELECT 2);
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM a7_ua WHERE a = (SELECT 1)
  UNION ALL
SELECT * FROM a7_ua WHERE a = (SELECT 2)) s;
SET LOCAL max_parallel_workers_per_gather = 0;
EXPLAIN SELECT * FROM a7_ua WHERE a = (SELECT 1)
  UNION ALL
SELECT * FROM a7_ua WHERE a = (SELECT 2);
COMMIT;
DROP TABLE a7_ua;
