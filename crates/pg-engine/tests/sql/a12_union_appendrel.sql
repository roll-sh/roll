-- A UNION ALL flattened into an appendrel: its parent range table entry (the FROM
-- subquery's alias, or `*SELECT* 1` for a query level's own UNION ALL) is the Append's and
-- takes its name first; arms kept as subqueries (a WHERE: is_safe_append_member) name their
-- relations after the level's. A LATERAL UNION ALL arm's NullTest of a NOT NULL column on a
-- join's preserved side is always true (restriction_is_always_true).
CREATE TABLE a12_ab (a int NOT NULL, b int NOT NULL) PARTITION BY LIST (a);
CREATE TABLE a12_ab_a1 PARTITION OF a12_ab FOR VALUES IN (1) PARTITION BY LIST (b);
CREATE TABLE a12_ab_a1_b1 PARTITION OF a12_ab_a1 FOR VALUES IN (1);
CREATE TABLE a12_ab_a1_b2 PARTITION OF a12_ab_a1 FOR VALUES IN (2);
CREATE TABLE a12_ab_a2 PARTITION OF a12_ab FOR VALUES IN (2) PARTITION BY LIST (b);
CREATE TABLE a12_ab_a2_b1 PARTITION OF a12_ab_a2 FOR VALUES IN (1);
CREATE TABLE a12_ab_a2_b2 PARTITION OF a12_ab_a2 FOR VALUES IN (2);
INSERT INTO a12_ab VALUES (1, 1), (1, 2), (2, 1), (2, 2);
CREATE TABLE a12_xy (x int, y int);
INSERT INTO a12_xy VALUES (1, 2), (5, 5);
ANALYZE a12_ab, a12_xy;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT * FROM a12_ab WHERE a = 1 UNION ALL SELECT * FROM a12_ab) a12_ab WHERE b = (SELECT 1);
SELECT * FROM (SELECT * FROM a12_ab WHERE a = 1 UNION ALL SELECT * FROM a12_ab) a12_ab WHERE b = (SELECT 1) ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT * FROM a12_ab WHERE a = 1 UNION ALL (VALUES (10, 5)) UNION ALL SELECT * FROM a12_ab) a12_ab
  WHERE b = (SELECT 1);
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT tableoid::regclass, a, b FROM a12_ab UNION ALL SELECT tableoid::regclass, x, y FROM a12_xy
  UNION ALL SELECT tableoid::regclass, a, b FROM a12_ab) a12_ab WHERE b = 2;
SELECT * FROM (SELECT tableoid::regclass, a, b FROM a12_ab UNION ALL SELECT tableoid::regclass, x, y FROM a12_xy
  UNION ALL SELECT tableoid::regclass, a, b FROM a12_ab) a12_ab WHERE b = 2 ORDER BY 1, 2, 3;
-- An alias no member's name: nothing changes.
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT * FROM a12_ab UNION ALL SELECT * FROM a12_ab) ss WHERE a = 2;
-- Arms of a query level's own UNION ALL.
EXPLAIN (COSTS OFF)
SELECT * FROM a12_ab WHERE a = 1 UNION ALL SELECT * FROM a12_ab UNION ALL SELECT x, y FROM a12_xy WHERE x > 1;
-- Joined to another table: the arm kept a subquery is named after the level's relations.
CREATE TABLE a12_z (z int);
INSERT INTO a12_z VALUES (1), (5);
ANALYZE a12_z;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a12_z, (SELECT * FROM a12_xy WHERE x = 5 UNION ALL SELECT * FROM a12_ab) a12_ab
  WHERE a12_ab.x = a12_z.z;
SELECT * FROM a12_z, (SELECT * FROM a12_xy WHERE x = 5 UNION ALL SELECT * FROM a12_ab) a12_ab
  WHERE a12_ab.x = a12_z.z ORDER BY 1, 2, 3;

CREATE TABLE a12_t (a int NOT NULL, b int);
INSERT INTO a12_t VALUES (1, 1), (2, 3), (3, NULL);
ANALYZE a12_t;
EXPLAIN (COSTS OFF)
SELECT t1.a, s.a FROM a12_t t1 LEFT JOIN a12_t t2 ON t1.a = t2.b
  JOIN LATERAL (SELECT t1.a AS a UNION ALL SELECT t2.a AS a) s ON true
  WHERE s.a IS NOT NULL;
SELECT t1.a, s.a FROM a12_t t1 LEFT JOIN a12_t t2 ON t1.a = t2.b
  JOIN LATERAL (SELECT t1.a AS a UNION ALL SELECT t2.a AS a) s ON true
  WHERE s.a IS NOT NULL ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT t1.a, s.a FROM a12_t t1 LEFT JOIN a12_t t2 ON t1.a = t2.b
  JOIN LATERAL (SELECT t1.a AS a UNION ALL SELECT t2.a AS a) s ON true
  WHERE s.a IS NULL;
SELECT t1.a, s.a FROM a12_t t1 LEFT JOIN a12_t t2 ON t1.a = t2.b
  JOIN LATERAL (SELECT t1.a AS a UNION ALL SELECT t2.a AS a) s ON true
  WHERE s.a IS NULL ORDER BY 1, 2;
-- A nullable column: the test stays.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.b, s.x FROM a12_t t1
  JOIN LATERAL (SELECT t1.b AS x UNION ALL SELECT t1.a AS x) s ON true
  WHERE s.x IS NOT NULL;
SELECT t1.b, s.x FROM a12_t t1
  JOIN LATERAL (SELECT t1.b AS x UNION ALL SELECT t1.a AS x) s ON true
  WHERE s.x IS NOT NULL ORDER BY 1, 2;
-- The nullable side of a left join under the LATERAL one: the test stays.
EXPLAIN (COSTS OFF)
SELECT t2.a, s.x FROM a12_t t1 LEFT JOIN a12_t t2 ON t1.a = t2.b
  JOIN LATERAL (SELECT t2.a AS x UNION ALL SELECT t2.a + 1) s ON true
  WHERE s.x IS NOT NULL;
SELECT t2.a, s.x FROM a12_t t1 LEFT JOIN a12_t t2 ON t1.a = t2.b
  JOIN LATERAL (SELECT t2.a AS x UNION ALL SELECT t2.a + 1) s ON true
  WHERE s.x IS NOT NULL ORDER BY 1, 2;
