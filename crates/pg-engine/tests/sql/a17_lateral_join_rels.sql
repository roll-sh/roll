-- Join relations parameterized by their LATERAL references (lateral_relids): a LATERAL
-- item joins an item it doesn't read first, the join's paths parameterized by the items
-- it still reads (join_is_legal's LATERAL rules, param_source_rels), and the nested loop
-- above passes them in, to a nested loop's or a hash join's input.
CREATE TABLE a17_i8 (q1 int8, q2 int8);
INSERT INTO a17_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
CREATE TABLE a17_i4 (f1 int4);
INSERT INTO a17_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
ANALYZE a17_i8;
ANALYZE a17_i4;
-- JSON_TABLE reading two items: joined with one of them first, parameterized by the other.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM generate_series(1, 3) x, generate_series(1, 3) y,
  JSON_TABLE(jsonb '[[1,2,3],[2,3,4,5],[3,4,5,6]]', 'strict $[*] ? (@[*] <= $x)'
    PASSING x AS x, y AS y
    COLUMNS (y text FORMAT JSON PATH '$',
      NESTED PATH 'strict $[*] ? (@ == $y)' COLUMNS (z int PATH '$'))) jt;
EXPLAIN
SELECT * FROM generate_series(1, 3) x, generate_series(1, 3) y,
  JSON_TABLE(jsonb '[[1,2,3],[2,3,4,5],[3,4,5,6]]', 'strict $[*] ? (@[*] <= $x)'
    PASSING x AS x, y AS y
    COLUMNS (y text FORMAT JSON PATH '$',
      NESTED PATH 'strict $[*] ? (@ == $y)' COLUMNS (z int PATH '$'))) jt;
SELECT * FROM generate_series(1, 3) x, generate_series(1, 3) y,
  JSON_TABLE(jsonb '[[1,2,3],[2,3,4,5],[3,4,5,6]]', 'strict $[*] ? (@[*] <= $x)'
    PASSING x AS x, y AS y
    COLUMNS (y text FORMAT JSON PATH '$',
      NESTED PATH 'strict $[*] ? (@ == $y)' COLUMNS (z int PATH '$'))) jt;
-- Inside an outer join: a parameterized Hash Right Join under the nested loop, the lower
-- outer join's clause moved into it as its filter.
EXPLAIN (COSTS OFF)
SELECT * FROM (VALUES (1, array[10,20]), (2, array[20,30])) AS v1(v1x, v1ys)
  LEFT JOIN (VALUES (1, 10), (2, 20)) AS v2(v2x, v2y) ON v2x = v1x
  LEFT JOIN unnest(v1ys) AS u1(u1y) ON u1y = v2y;
SELECT * FROM (VALUES (1, array[10,20]), (2, array[20,30])) AS v1(v1x, v1ys)
  LEFT JOIN (VALUES (1, 10), (2, 20)) AS v2(v2x, v2y) ON v2x = v1x
  LEFT JOIN unnest(v1ys) AS u1(u1y) ON u1y = v2y
  ORDER BY 1, 3, 5;
-- unnest of a column: estimate_array_length's 10 rows; of a null constant, none.
CREATE TABLE a17_arr (k int, a int[]);
EXPLAIN SELECT * FROM a17_arr t, unnest(t.a) u;
EXPLAIN SELECT * FROM unnest(NULL::int[]) u;
DROP TABLE a17_arr;
-- A LATERAL subquery pulled up under an outer join: its value is computed at its scan, the
-- hash join's inner side, parameterized by the item it reads.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a17_i8 a,
  a17_i8 x LEFT JOIN LATERAL (SELECT a.q1 FROM a17_i4 y) ss(z) ON x.q2 = ss.z
  ORDER BY a.q1, a.q2, x.q1, x.q2, ss.z;
SELECT * FROM a17_i8 a,
  a17_i8 x LEFT JOIN LATERAL (SELECT a.q1 FROM a17_i4 y) ss(z) ON x.q2 = ss.z
  ORDER BY a.q1, a.q2, x.q1, x.q2, ss.z;
-- A function's join clause over an item it doesn't read stays the join's.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT 1 AS x OFFSET 0) x CROSS JOIN (SELECT 2 AS y OFFSET 0) y
  LEFT JOIN LATERAL (SELECT * FROM (SELECT 3 AS z OFFSET 0) z WHERE z.z = x.x) zz ON zz.z = y.y;
SELECT * FROM (VALUES (1, array[1,2,3]), (2, array[2,5])) AS a(k, arr)
  CROSS JOIN (VALUES (2), (3)) AS b(m)
  LEFT JOIN LATERAL unnest(a.arr) AS u(e) ON u.e = b.m
  ORDER BY 1, 3, 4;
-- A chain of LATERAL references: c reads b, b reads a.
EXPLAIN (COSTS OFF)
SELECT * FROM a17_i4 a, LATERAL (SELECT a.f1 + 1 AS g OFFSET 0) b,
  LATERAL (SELECT b.g * 2 AS h OFFSET 0) c, a17_i8 d
  WHERE d.q1 = c.h;
SELECT * FROM a17_i4 a, LATERAL (SELECT a.f1 + 1 AS g OFFSET 0) b,
  LATERAL (SELECT b.g * 2 AS h OFFSET 0) c, a17_i8 d
  WHERE abs(a.f1) < 1000000 AND (d.q1 = c.h OR c.h = 2) ORDER BY 1, 4, 5;
DROP TABLE a17_i8, a17_i4;
