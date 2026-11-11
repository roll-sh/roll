-- PlaceHolderVars holding sub-selects (join 362 / 363): a pulled-up subquery whose
-- sub-select output an outer join nulls is still pulled up (its output a PlaceHolderVar),
-- its LATERAL item's relations joined in the parent's join problem; a nested loop's
-- PlaceHolderVar parameter evaluated over the outer side and the join path's own
-- parameterization (identify_current_nestloop_params); a LATERAL subquery's reference to
-- the PlaceHolderVar gets an unused SubPlan copy (extract_lateral_references'
-- preprocess_phv_expression, the unreferenced InitPlan); a LATERAL subquery keeps its
-- lateral references when the outputs reading them aren't read; unread constant outputs of
-- a subquery are NULLs (remove_unused_subquery_outputs).
CREATE TABLE a52_i8 (q1 int8, q2 int8);
INSERT INTO a52_i8 VALUES ('123', '456'), ('123', '4567890123456789'),
  ('4567890123456789', '123'), ('4567890123456789', '4567890123456789'),
  ('4567890123456789', '-4567890123456789');
CREATE TABLE a52_i4 (f1 int4);
INSERT INTO a52_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
ANALYZE a52_i8;
ANALYZE a52_i4;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4
WHERE i8.q2 = 123;
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4
WHERE i8.q2 = 123;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a52_i4 i4, LATERAL (SELECT 1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4
WHERE i8.q2 = 123;
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a52_i4 i4, LATERAL (SELECT 1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4
WHERE i8.q2 = 123;

-- Not reduced: the right join's null-extended row, the LATERAL subquery reading a NULL x.
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 1) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x LIMIT 1) ss4;
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x, y
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l, x AS xx WHERE x LIMIT 1) ss4
ORDER BY 1, 2;
-- Two references in the LATERAL subquery: two unused copies.
SELECT *
FROM a52_i8 i8
  INNER JOIN
    (SELECT (SELECT true) AS x, y
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1
       WHERE i4.f1 = 0) ss2 ON true
  RIGHT JOIN (SELECT false AS z) ss3 ON true,
  LATERAL (SELECT i8.q2 AS q2l WHERE x AND x LIMIT 1) ss4
WHERE i8.q2 = 123;

-- A correlated sub-select in the PlaceHolderVar, null-extended rows of the left join.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT *
FROM a52_i8 i8
  LEFT JOIN
    (SELECT (SELECT i4.f1 > 0) AS x, i4.f1
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1) ss2 ON i8.q2 = ss2.f1,
  LATERAL (SELECT i8.q1 AS q1l WHERE x IS NOT false LIMIT 1) ss4;
SELECT *
FROM a52_i8 i8
  LEFT JOIN
    (SELECT (SELECT i4.f1 > 0) AS x, i4.f1
       FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) ss1) ss2 ON i8.q2 = ss2.f1,
  LATERAL (SELECT i8.q1 AS q1l WHERE x IS NOT false LIMIT 1) ss4
ORDER BY 1, 2;

-- A pulled-up subquery's sub-select output under an outer join, no LATERAL item.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT i8.q1, ss.x, ss.f1
FROM a52_i8 i8 LEFT JOIN (SELECT (SELECT max(f1) FROM a52_i4) AS x, i4.f1
                          FROM a52_i4 i4, a52_i4 i5 WHERE i4.f1 = i5.f1) ss ON i8.q2 = ss.f1;
SELECT i8.q1, ss.x, ss.f1
FROM a52_i8 i8 LEFT JOIN (SELECT (SELECT max(f1) FROM a52_i4) AS x, i4.f1
                          FROM a52_i4 i4, a52_i4 i5 WHERE i4.f1 = i5.f1) ss ON i8.q2 = ss.f1
ORDER BY 1, 3;

-- A LATERAL subquery whose only reference is in an output nothing reads: still
-- parameterized by it.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT i4.f1 FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) s;
SELECT i4.f1 FROM a52_i4 i4, LATERAL (SELECT i4.f1 AS y LIMIT 1) s ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT i4.f1 FROM a52_i4 i4 LEFT JOIN LATERAL (SELECT i4.f1 AS y, 2 AS z LIMIT 1) s ON true;
SELECT i4.f1, s.z FROM a52_i4 i4
  LEFT JOIN LATERAL (SELECT i4.f1 AS y, 2 AS z WHERE i4.f1 > 0 LIMIT 1) s ON true
ORDER BY 1;

-- Unread constant outputs of subqueries planned apart are NULLs.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t.f1 FROM a52_i4 t, (SELECT 1 AS c, f1 FROM a52_i4 LIMIT 2) s WHERE s.f1 = t.f1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT s.f1 FROM (SELECT 'a'::text AS c, f1 FROM a52_i4 ORDER BY f1 LIMIT 2) s;
SELECT s.f1 FROM (SELECT 'a'::text AS c, f1 FROM a52_i4 ORDER BY f1 LIMIT 2) s;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*) FROM (SELECT 1 AS one, f1 FROM a52_i4 OFFSET 0) s;
SELECT count(*) FROM (SELECT 1 AS one, f1 FROM a52_i4 OFFSET 0) s;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t.f1 FROM a52_i4 t JOIN (SELECT f1, f1 + 1 AS g FROM a52_i4 OFFSET 0) s ON t.f1 = s.f1;
SELECT t.f1 FROM a52_i4 t JOIN (SELECT f1, f1 + 1 AS g FROM a52_i4 OFFSET 0) s ON t.f1 = s.f1
ORDER BY 1;

DROP TABLE a52_i8;
DROP TABLE a52_i4;
