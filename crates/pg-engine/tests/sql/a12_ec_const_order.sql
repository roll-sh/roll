-- generate_base_implied_equalities_const: a class with a constant and more than one other
-- member gives each member `member = constant` (written `constant = member` or not), the
-- classes' clauses in the order the classes were made; a class of one clause keeps it as
-- written, at its class's place.
CREATE TABLE a12_sj (a int UNIQUE, b int, c int UNIQUE);
INSERT INTO a12_sj VALUES (1, NULL, 2), (NULL, 2, NULL), (2, 1, 1), (1, 1, 3), (2, 2, 4);
ANALYZE a12_sj;
CREATE UNIQUE INDEX a12_sj_abc ON a12_sj (a, b, c);
EXPLAIN (COSTS OFF)
SELECT * FROM a12_sj j1, a12_sj j2
  WHERE j1.b = j2.b AND 2 = j1.a AND j1.c = 3 AND j2.a = 1 AND 3 = j2.c;
SELECT * FROM a12_sj j1, a12_sj j2
  WHERE j1.b = j2.b AND 2 = j1.a AND j1.c = 3 AND j2.a = 1 AND 3 = j2.c;
CREATE TABLE a12_t1 (x int, y int, z int);
CREATE TABLE a12_t2 (x int, y int, z int);
INSERT INTO a12_t1 SELECT g, g % 7, g % 5 FROM generate_series(1, 200) g;
INSERT INTO a12_t2 SELECT g, g % 3, g % 5 FROM generate_series(1, 200) g;
ANALYZE a12_t1, a12_t2;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a12_t1 t1, a12_t2 t2
  WHERE t1.x = t2.x AND 4 = t1.z AND t2.y = 1 AND 4 = t2.z AND t1.y = 3;
SELECT count(*) FROM a12_t1 t1, a12_t2 t2
  WHERE t1.x = t2.x AND 4 = t1.z AND t2.y = 1 AND 4 = t2.z AND t1.y = 3;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a12_t1 t1, a12_t2 t2
  WHERE 5 = t2.x AND t1.x = t2.x AND t1.y = 2 AND 2 = t2.y;
SELECT count(*) FROM a12_t1 t1, a12_t2 t2
  WHERE 5 = t2.x AND t1.x = t2.x AND t1.y = 2 AND 2 = t2.y;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a12_t1 t1, a12_t2 t2
  WHERE t1.z = t2.z AND 1 = t1.x AND t2.y = 0 AND 1 = t2.x;
SELECT count(*) FROM a12_t1 t1, a12_t2 t2
  WHERE t1.z = t2.z AND 1 = t1.x AND t2.y = 0 AND 1 = t2.x;
