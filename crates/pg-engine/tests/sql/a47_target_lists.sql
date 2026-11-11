-- Target lists PostgreSQL builds from attr_needed (build_joinrel_tlist), resjunk sort
-- columns in the final target list, appendrel reltargets over inheritance Appends,
-- relabelings kept on clauses translated onto appendrel members, and FROM-less relations
-- of outer joins reduced to inner ones (remove_useless_result_rtes).
CREATE TABLE a47_pt (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE a47_pt_1 PARTITION OF a47_pt FOR VALUES FROM (0) TO (100) PARTITION BY RANGE (b);
CREATE TABLE a47_pt_1_1 PARTITION OF a47_pt_1 FOR VALUES FROM (0) TO (100);
CREATE TABLE a47_pt_2 PARTITION OF a47_pt FOR VALUES FROM (100) TO (200);
INSERT INTO a47_pt VALUES (1, 1, 'x'), (101, 101, 'y'), (102, 102, 'w');
CREATE TABLE a47_ut (a int, b int, c varchar);
INSERT INTO a47_ut VALUES (101, 101, 'y'), (2, 2, 'z'), (102, 102, 'v'), (NULL, NULL, NULL);
CREATE TABLE a47_i8 (q1 int8, q2 int8);
INSERT INTO a47_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123);
CREATE TABLE a47_i4 (f1 int4);
INSERT INTO a47_i4 VALUES (0), (123456), (-123456);
ANALYZE a47_pt;
ANALYZE a47_ut;
ANALYZE a47_i8;
ANALYZE a47_i4;

-- A LATERAL subquery over a join pulled up: the ON clause's Var stays in the join
-- relation's target list though a parameterized scan below checks the clause, the ORDER BY
-- column last in the Sort's and the top join's lists.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.b, ss.phv FROM a47_ut t1 LEFT JOIN LATERAL
  (SELECT t2.a AS t2a, t3.a t3a, least(t1.a, t2.a, t3.b) phv
     FROM a47_pt t2 JOIN a47_ut t3 ON t2.a = t3.b) ss
  ON t1.a = ss.t2a ORDER BY t1.a;
SELECT t1.b, ss.phv FROM a47_ut t1 LEFT JOIN LATERAL
  (SELECT t2.a AS t2a, t3.a t3a, least(t1.a, t2.a, t3.b) phv
     FROM a47_pt t2 JOIN a47_ut t3 ON t2.a = t3.b) ss
  ON t1.a = ss.t2a ORDER BY t1.a;
SELECT t1.b, ss.phv, ss.t3a FROM a47_ut t1 LEFT JOIN LATERAL
  (SELECT t2.a AS t2a, t3.a t3a, coalesce(t1.a, t2.a, t3.a) phv
     FROM a47_pt t2 JOIN a47_ut t3 ON t2.a = t3.b) ss
  ON t1.a = ss.t2a AND t1.c = ss.t2a::text ORDER BY t1.b NULLS FIRST;

-- The varchar comparison's relabelings on the appendrel members' parameterized filters.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.b, ss.phv FROM a47_ut t1 LEFT JOIN LATERAL
  (SELECT t2.a AS t2a, t2.c AS t2c, least(t1.a, t2.a, t3.b) phv
     FROM a47_pt t2 JOIN a47_ut t3 ON t2.a = t3.b) ss
  ON t1.a = ss.t2a AND t1.c = ss.t2c ORDER BY t1.a;
SELECT t1.b, ss.phv FROM a47_ut t1 LEFT JOIN LATERAL
  (SELECT t2.a AS t2a, t2.c AS t2c, least(t1.a, t2.a, t3.b) phv
     FROM a47_pt t2 JOIN a47_ut t3 ON t2.a = t3.b) ss
  ON t1.a = ss.t2a AND t1.c = ss.t2c ORDER BY t1.a;

-- A Sort, a Hash or a Materialize over an inheritance tree's Append returns the
-- appendrel's reltarget.
SET enable_hashjoin = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t3.c FROM a47_ut t3 JOIN a47_pt t2 ON t3.b = t2.a;
SELECT t3.c FROM a47_ut t3 JOIN a47_pt t2 ON t3.b = t2.a ORDER BY 1;
RESET enable_hashjoin;
SET enable_mergejoin = off;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t3.c FROM a47_ut t3 JOIN a47_pt t2 ON t3.b = t2.a;
SELECT t3.c FROM a47_ut t3 JOIN a47_pt t2 ON t3.b = t2.a ORDER BY 1;
RESET enable_mergejoin;

-- An outer join reduce_outer_joins made inner with a FROM-less relation: that side is
-- removed, its values constants.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (a47_i8 i8 INNER JOIN a47_i4 i4 ON true) RIGHT JOIN (SELECT false AS z) ss3 ON true
  WHERE i8.q2 = 123;
SELECT * FROM (a47_i8 i8 INNER JOIN a47_i4 i4 ON true) RIGHT JOIN (SELECT false AS z) ss3 ON true
  WHERE i8.q2 = 123 ORDER BY i4.f1;
SELECT * FROM (a47_i8 i8 INNER JOIN a47_i4 i4 ON true) RIGHT JOIN (SELECT false AS z) ss3 ON true
  WHERE i8.q2 = 1;
-- (Not reduced: the FROM-less side's row stays, null-extended.)
SELECT * FROM (a47_i8 i8 INNER JOIN a47_i4 i4 ON i4.f1 = 999) RIGHT JOIN (SELECT false AS z) ss3 ON true
  WHERE i8.q2 IS NULL;
SELECT * FROM (a47_i8 i8 INNER JOIN a47_i4 i4 ON i4.f1 = 999) RIGHT JOIN (SELECT false AS z) ss3 ON true;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (a47_i8 i8 INNER JOIN (SELECT (SELECT true) AS x FROM a47_i4 i4 WHERE i4.f1 = 0) ss2 ON true)
  RIGHT JOIN (SELECT false AS z) ss3 ON true WHERE i8.q2 = 123;
SELECT * FROM (a47_i8 i8 INNER JOIN (SELECT (SELECT true) AS x FROM a47_i4 i4 WHERE i4.f1 = 0) ss2 ON true)
  RIGHT JOIN (SELECT false AS z) ss3 ON true WHERE i8.q2 = 123;
SELECT * FROM (a47_i8 i8 INNER JOIN (SELECT (SELECT true) AS x FROM a47_i4 i4 WHERE i4.f1 = 1) ss2 ON true)
  RIGHT JOIN (SELECT false AS z) ss3 ON true WHERE i8.q2 = 123;

-- A UNION ALL's FROM-less arm as a member of the appendrel's parameterized Append
-- (reparameterize_path's RTE_RESULT case): a Result whose One-Time Filter is the join
-- clause, the outer member first.
CREATE TABLE a47_k (i int PRIMARY KEY);
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT i FROM a47_k UNION ALL SELECT 0) s JOIN (SELECT 1 AS j LIMIT 1) ON i = j;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT i FROM a47_k UNION ALL SELECT 0 UNION ALL SELECT 1) s JOIN (SELECT 1 AS j LIMIT 1) ON i = j;
INSERT INTO a47_k VALUES (0), (1), (2);
SELECT * FROM (SELECT i FROM a47_k UNION ALL SELECT 0) s JOIN (SELECT 1 AS j LIMIT 1) ON i = j;
SELECT * FROM (SELECT i FROM a47_k UNION ALL SELECT 0) s JOIN (SELECT 0 AS j LIMIT 1) ON i = j ORDER BY 1;
SELECT * FROM (SELECT i FROM a47_k UNION ALL SELECT 0 UNION ALL SELECT 1) s JOIN (SELECT 1 AS j LIMIT 1) ON i = j;
SELECT * FROM (SELECT j FROM (VALUES (0), (1), (5)) v(j) LIMIT 3) v
  LEFT JOIN (SELECT i FROM a47_k UNION ALL SELECT 0) s ON i = j ORDER BY 1, 2;
DROP TABLE a47_k;
DROP TABLE a47_pt;
DROP TABLE a47_ut;
DROP TABLE a47_i8;
DROP TABLE a47_i4;
