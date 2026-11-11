-- PlaceHolderVars evaluated at joins: a subquery over a join pulled up under an outer join
-- (as written) keeps its outputs that wouldn't go NULL with its rows (constants,
-- non-strict expressions) as PlaceHolderVars, computed at the lowest join of the relations
-- they read (or of all the subquery's), read above it as columns and nulled with the rows
-- outer joins null; in the outer join's own ON clause they are the expressions themselves.
CREATE TABLE a8_pa (f1 int);
INSERT INTO a8_pa VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
CREATE TABLE a8_pb (q1 int8, q2 int8);
INSERT INTO a8_pb VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
CREATE TABLE a8_one ();
INSERT INTO a8_one DEFAULT VALUES;
CREATE TABLE a8_big (unique1 int, unique2 int, stringu1 name, stringu2 name);
INSERT INTO a8_big SELECT i, (i * 7) % 1000, 'S' || (i % 17), 'T' || (i % 13) FROM generate_series(0, 999) i;
CREATE UNIQUE INDEX a8_big_u1 ON a8_big (unique1);
CREATE UNIQUE INDEX a8_big_u2 ON a8_big (unique2);
ANALYZE a8_pa;
ANALYZE a8_pb;
ANALYZE a8_one;
ANALYZE a8_big;
-- The join computes the PlaceHolderVars; the ON clause reads the constant itself.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_pa i1
  LEFT JOIN (SELECT v1.x2, v2.y1, 11 AS d1
             FROM (SELECT 1, 0 FROM a8_one) v1(x1, x2)
             LEFT JOIN (SELECT 3, 1 FROM a8_one) v2(y1, y2)
             ON v1.x1 = v2.y2) subq1
  ON (i1.f1 = subq1.x2);
SELECT * FROM a8_pa i1
  LEFT JOIN (SELECT v1.x2, v2.y1, 11 AS d1
             FROM (SELECT 1, 0 FROM a8_one) v1(x1, x2)
             LEFT JOIN (SELECT 3, 1 FROM a8_one) v2(y1, y2)
             ON v1.x1 = v2.y2) subq1
  ON (i1.f1 = subq1.x2)
ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c FROM a8_pa a JOIN a8_pa b ON a.f1 = b.f1) s
  ON i1.f1 = s.c;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c FROM a8_pa a JOIN a8_pa b ON a.f1 = b.f1) s
  ON i1.f1 = s.c + 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c FROM a8_pa a JOIN a8_pa b ON a.f1 = b.f1) s
  ON i1.f1 = s.x AND s.c > 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c FROM a8_pa a JOIN a8_pa b ON a.f1 = b.f1) s
  ON i1.f1 = s.c WHERE s.x = 1;
-- PlaceHolderVars passed as parameters.
SELECT t1.unique2, t1.stringu1, t2.unique1, t2.stringu2 FROM
  a8_big t1
  INNER JOIN a8_pa i1
    LEFT JOIN (SELECT v1.x2, v2.y1, 11 AS d1
               FROM (SELECT 1, 0 FROM a8_one) v1(x1, x2)
               LEFT JOIN (SELECT 3, 1 FROM a8_one) v2(y1, y2)
               ON v1.x1 = v2.y2) subq1
    ON (i1.f1 = subq1.x2)
  ON (t1.unique2 = subq1.d1)
  LEFT JOIN a8_big t2
  ON (subq1.y1 = t2.unique1)
WHERE t1.unique2 < 42 AND t1.stringu1 > t2.stringu2;
CREATE TABLE a8_nt1 (id int PRIMARY KEY, a1 boolean, a2 boolean);
CREATE TABLE a8_nt2 (id int PRIMARY KEY, nt1_id int, b1 boolean, b2 boolean);
CREATE TABLE a8_nt3 (id int PRIMARY KEY, nt2_id int, c1 boolean);
INSERT INTO a8_nt1 VALUES (1, true, true), (2, true, false), (3, false, false);
INSERT INTO a8_nt2 VALUES (1, 1, true, true), (2, 2, true, false), (3, 3, false, false);
INSERT INTO a8_nt3 VALUES (1, 1, true), (2, 2, false), (3, 3, true);
-- A clause over a PlaceHolderVar a parameterized scan evaluates: one clause, unsplit.
EXPLAIN (COSTS OFF)
SELECT nt3.id
FROM a8_nt3 AS nt3
  LEFT JOIN
    (SELECT nt2.*, (nt2.b1 AND ss1.a3) AS b3
     FROM a8_nt2 AS nt2
       LEFT JOIN
         (SELECT nt1.*, (nt1.id IS NOT NULL) AS a3 FROM a8_nt1 AS nt1) AS ss1
         ON ss1.id = nt2.nt1_id
    ) AS ss2
    ON ss2.id = nt3.nt2_id
WHERE nt3.id = 1 AND ss2.b3;
SELECT nt3.id, ss2.* FROM a8_nt3 nt3
  LEFT JOIN (SELECT nt2.*, (nt2.b1 AND ss1.a3) AS b3
             FROM a8_nt2 nt2 LEFT JOIN (SELECT nt1.*, (nt1.id IS NOT NULL) AS a3 FROM a8_nt1 nt1) AS ss1
             ON ss1.id = nt2.nt1_id) AS ss2
  ON ss2.id = nt3.nt2_id ORDER BY 1;
SELECT nt3.id, ss2.b3 FROM a8_nt3 nt3
  LEFT JOIN (SELECT nt2.*, (nt2.b1 OR ss1.a3) AS b3
             FROM a8_nt2 nt2 LEFT JOIN (SELECT nt1.*, (nt1.id > 1) AS a3 FROM a8_nt1 nt1) AS ss1
             ON ss1.id = nt2.nt1_id + 1) AS ss2
  ON ss2.id = nt3.nt2_id WHERE ss2.b3 IS NOT TRUE ORDER BY 1;
-- Results: the PlaceHolderVars go NULL with their rows.
SELECT i1.f1, s.* FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c, coalesce(b.f1, -1) AS cb
             FROM a8_pa a LEFT JOIN a8_pa b ON a.f1 = b.f1 + 0) s
  ON i1.f1 = s.x ORDER BY 1, 2;
SELECT i1.f1, s.c, s.x FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c FROM a8_pa a JOIN a8_pa b ON a.f1 = b.f1 WHERE a.f1 > 0) s
  ON i1.f1 = s.x WHERE s.c IS NULL ORDER BY 1;
SELECT i1.f1, s.c, i2.f1 FROM a8_pa i1
  LEFT JOIN (SELECT a.f1 AS x, 5 AS c FROM a8_pa a JOIN a8_pa b ON a.f1 = b.f1 WHERE a.f1 > 0) s
  ON i1.f1 = s.x
  LEFT JOIN a8_pa i2 ON i2.f1 = s.c ORDER BY 1, 3;
SELECT q1, ss.* FROM a8_pb t
  LEFT JOIN (SELECT i.f1, coalesce(j.q2, 42) AS z, (i.f1 IS NULL) AS n
             FROM a8_pa i LEFT JOIN a8_pb j ON i.f1 = j.q1) ss
  ON t.q2 = ss.f1 ORDER BY 1, 2, 3;
SELECT count(*), count(ss.t), count(ss.s) FROM a8_pb t0
  LEFT JOIN (SELECT a.q1, 'x'::text AS t, coalesce(b.q2, 0) AS s
             FROM a8_pb a LEFT JOIN a8_pb b ON a.q2 = b.q1 AND b.q1 > 1000) ss ON t0.q1 = ss.q1 + 1;
SELECT t0.q1, ss.* FROM a8_pb t0
  LEFT JOIN (SELECT a.q1, 'x'::text AS t, coalesce(b.q2, 0) AS s
             FROM a8_pb a LEFT JOIN a8_pb b ON a.q2 = b.q1) ss ON t0.q1 = ss.q1 AND ss.s > 100
  ORDER BY 1, 2, 4;
DROP TABLE a8_nt3;
DROP TABLE a8_nt2;
DROP TABLE a8_nt1;
DROP TABLE a8_big;
DROP TABLE a8_one;
DROP TABLE a8_pb;
DROP TABLE a8_pa;
