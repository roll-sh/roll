-- A FROM-less subquery's non-volatile function call is pulled up (its equality a
-- restriction of the other side, an equivalence class member across the join above, so a
-- partitioned target is pruned as the executor starts); MERGE through a view names the
-- view's relation; merge joins on an expression of one side (a FULL JOIN USING's merged
-- column, a COALESCE); a RIGHT JOIN's left-join form first; the first derivation of an
-- equivalence class clause fixes its orientation; partitionwise joins under a LIMIT.
SET plan_cache_mode = force_generic_plan;
CREATE TABLE a5_abc (a int, b text, c bool) PARTITION BY LIST (a);
CREATE TABLE a5_abc_1 (b text, a int, c bool);
CREATE TABLE a5_abc_2 (a int, c bool, b text);
ALTER TABLE a5_abc ATTACH PARTITION a5_abc_1 FOR VALUES IN (1);
ALTER TABLE a5_abc ATTACH PARTITION a5_abc_2 FOR VALUES IN (2);
INSERT INTO a5_abc VALUES (1, 'b', true), (2, 'c', true);
CREATE VIEW a5_abc_view AS SELECT * FROM a5_abc WHERE b <> 'a' WITH CHECK OPTION;
CREATE FUNCTION a5_stable_one() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql STABLE;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT a5_stable_one() AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a);
SELECT * FROM (SELECT a5_stable_one() AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a);
EXPLAIN (COSTS OFF)
SELECT * FROM a5_abc pt JOIN ((SELECT a5_stable_one() AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a)) ON pt.a = pt1.a;
SELECT * FROM a5_abc pt JOIN ((SELECT a5_stable_one() AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a)) ON pt.a = pt1.a;
EXPLAIN (COSTS OFF)
MERGE INTO a5_abc_view pt
USING (SELECT a5_stable_one() AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a) ON pt.a = pt1.a
WHEN MATCHED THEN DELETE RETURNING pt.a;
BEGIN;
MERGE INTO a5_abc_view pt
USING (SELECT a5_stable_one() AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a) ON pt.a = pt1.a
WHEN MATCHED THEN DELETE RETURNING pt.a;
TABLE a5_abc_view;
ROLLBACK;
EXPLAIN (COSTS OFF)
MERGE INTO a5_abc_view pt
USING (SELECT a5_stable_one() + 2 AS pid) AS q JOIN a5_abc_1 pt1 ON (q.pid = pt1.a) ON pt.a = pt1.a
WHEN MATCHED THEN DELETE RETURNING pt.a;
EXPLAIN (COSTS OFF)
MERGE INTO a5_abc_view pt
USING (SELECT a5_stable_one() + 1 AS pid) AS q JOIN a5_abc_2 pt2 ON (q.pid = pt2.a) ON pt.a = a5_stable_one() + 2
WHEN NOT MATCHED THEN INSERT VALUES (1, 'd', false) RETURNING pt.a;
BEGIN;
MERGE INTO a5_abc_view pt
USING (SELECT a5_stable_one() + 1 AS pid) AS q JOIN a5_abc_2 pt2 ON (q.pid = pt2.a) ON pt.a = a5_stable_one() + 2
WHEN NOT MATCHED THEN INSERT VALUES (1, 'd', false) RETURNING pt.a;
TABLE a5_abc_view;
ROLLBACK;
CREATE TABLE a5_t1 (name text, n int);
CREATE TABLE a5_t2 (name text, n int);
CREATE TABLE a5_t3 (name text, n int);
INSERT INTO a5_t1 VALUES ('bb', 11);
INSERT INTO a5_t2 VALUES ('bb', 12), ('cc', 22), ('ee', 42);
INSERT INTO a5_t3 VALUES ('bb', 13), ('cc', 23), ('dd', 33);
EXPLAIN (COSTS OFF) SELECT * FROM a5_t1 FULL JOIN a5_t2 USING (name) FULL JOIN a5_t3 USING (name);
SELECT * FROM a5_t1 FULL JOIN a5_t2 USING (name) FULL JOIN a5_t3 USING (name);
CREATE TABLE a5_i8 (q1 int8, q2 int8);
INSERT INTO a5_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
ANALYZE a5_i8;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a.q2, b.q1 FROM a5_i8 a LEFT JOIN a5_i8 b ON a.q2 = coalesce(b.q1, 1) WHERE coalesce(b.q1, 1) > 0;
SELECT a.q2, b.q1 FROM a5_i8 a LEFT JOIN a5_i8 b ON a.q2 = coalesce(b.q1, 1) WHERE coalesce(b.q1, 1) > 0;
CREATE TABLE a5_j1 (id int PRIMARY KEY);
CREATE TABLE a5_j2 (id int PRIMARY KEY);
INSERT INTO a5_j1 VALUES (1), (2), (3);
INSERT INTO a5_j2 VALUES (1), (2), (3);
ANALYZE a5_j1;
ANALYZE a5_j2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a5_j1 RIGHT JOIN a5_j2 ON a5_j1.id = a5_j2.id;
CREATE TABLE a5_k1 (id1 int, id2 int, PRIMARY KEY (id1, id2));
CREATE TABLE a5_k2 (id1 int, id2 int, PRIMARY KEY (id1, id2));
INSERT INTO a5_k1 VALUES (1, 1), (1, 2);
INSERT INTO a5_k2 VALUES (1, 1);
ANALYZE a5_k1;
ANALYZE a5_k2;
CREATE UNIQUE INDEX a5_k1_id2_idx ON a5_k1 (id2) WHERE id2 IS NOT NULL;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a5_k1 INNER JOIN a5_k2 ON a5_k1.id2 = a5_k2.id2;
DROP INDEX a5_k1_id2_idx;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a5_k1 INNER JOIN a5_k2 ON a5_k1.id2 = a5_k2.id2;
SET enable_partitionwise_join = on;
CREATE TABLE a5_pht (a int, b int, c text) PARTITION BY HASH (c);
CREATE TABLE a5_pht_p1 PARTITION OF a5_pht FOR VALUES WITH (MODULUS 3, REMAINDER 0);
CREATE TABLE a5_pht_p2 PARTITION OF a5_pht FOR VALUES WITH (MODULUS 3, REMAINDER 1);
CREATE TABLE a5_pht_p3 PARTITION OF a5_pht FOR VALUES WITH (MODULUS 3, REMAINDER 2);
INSERT INTO a5_pht SELECT i, i, to_char(i/50, 'FM0000') FROM generate_series(0, 599, 2) i;
ANALYZE a5_pht;
CREATE INDEX a5_pht_c_idx ON a5_pht (c);
EXPLAIN (COSTS OFF) SELECT * FROM a5_pht p1 JOIN a5_pht p2 USING (c) LIMIT 1;
EXPLAIN (COSTS OFF) SELECT * FROM a5_pht p1 JOIN a5_pht p2 USING (c) LIMIT 100;
EXPLAIN (COSTS OFF) SELECT * FROM a5_pht p1 JOIN a5_pht p2 USING (c) LIMIT 1000;
SELECT count(*) FROM (SELECT * FROM a5_pht p1 JOIN a5_pht p2 USING (c) LIMIT 100) s;
RESET enable_partitionwise_join;
DROP TABLE a5_abc, a5_t1, a5_t2, a5_t3, a5_i8, a5_j1, a5_j2, a5_k1, a5_k2, a5_pht CASCADE;
DROP FUNCTION a5_stable_one();
