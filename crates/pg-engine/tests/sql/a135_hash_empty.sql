-- Hash joins skip a side as ExecHashJoin does (agent 135): an empty outer side builds
-- nothing (the inner side's failing expression never runs), an empty inner side leaves the
-- outer side unread (its failing filter never runs) unless the join fills it, and EXPLAIN
-- ANALYZE shows the Hash never executed. difftest seed 5069 (DELETE ... USING a view).
CREATE TABLE a135_e (id int, k int);
CREATE TABLE a135_s (k int, z int);
INSERT INTO a135_s SELECT i, i % 5 FROM generate_series(1, 2000) i;
CREATE VIEW a135_v AS SELECT k, 10 / z AS q FROM a135_s;
ANALYZE a135_e;
ANALYZE a135_s;
SET enable_mergejoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF) DELETE FROM a135_e USING a135_v WHERE a135_e.k = a135_v.k AND a135_v.q > 0;
DELETE FROM a135_e USING a135_v WHERE a135_e.k = a135_v.k AND a135_v.q > 0;
EXPLAIN (COSTS OFF) SELECT e.id, v.q FROM a135_e e JOIN a135_v v ON v.k = e.k;
SELECT e.id, v.q FROM a135_e e JOIN a135_v v ON v.k = e.k;
SELECT e.id, v.q FROM a135_e e LEFT JOIN a135_v v ON v.k = e.k;
SELECT count(*) FROM a135_e e WHERE NOT EXISTS (SELECT 1 FROM a135_v v WHERE v.k = e.k);
-- the inner side empty: the outer side's failing filter isn't reached
EXPLAIN (COSTS OFF) SELECT s.k FROM a135_s s JOIN a135_e e ON e.k = s.k WHERE 10 / s.z > 1;
SELECT s.k FROM a135_s s JOIN a135_e e ON e.k = s.k WHERE 10 / s.z > 1;
SELECT count(*) FROM a135_s s WHERE EXISTS (SELECT 1 FROM a135_e e WHERE e.k = s.k) AND 10 / s.z > 1;
-- a left join reads its outer side all the same
SELECT count(*) FROM a135_s s LEFT JOIN a135_e e ON e.k = s.k WHERE 10 / s.z > 1;
SELECT count(*) FROM a135_s s LEFT JOIN a135_e e ON e.k = s.k WHERE 10 / (s.z + 1) > 1;
-- both sides with rows: the error comes out
INSERT INTO a135_e VALUES (1, 5), (2, 7);
SELECT e.id, v.q FROM a135_e e JOIN a135_v v ON v.k = e.k;
DELETE FROM a135_e;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT e.id, v.q FROM a135_e e JOIN a135_v v ON v.k = e.k;
RESET enable_mergejoin;
RESET enable_nestloop;
DROP VIEW a135_v;
DROP TABLE a135_e;
DROP TABLE a135_s;
