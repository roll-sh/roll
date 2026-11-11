-- Equalities above a left join, read in its nullable side: one equivalence class, its
-- clauses derived at the join (the members the join computes equated in turn, the last
-- with an outer side member; a written clause over the same two members reused as written).
CREATE TABLE a5_oa (q1 int8, q2 int8);
INSERT INTO a5_oa VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789), (5, 5);
CREATE TABLE a5_ob (unique2 int, thousand int, hundred int, ten int);
INSERT INTO a5_ob SELECT i, i % 1000, i % 100, i % 10 FROM generate_series(0, 9999) i;
CREATE UNIQUE INDEX a5_ob_u2 ON a5_ob (unique2);
VACUUM ANALYZE a5_oa;
VACUUM ANALYZE a5_ob;
SET enable_memoize = off;
SELECT q1, unique2, thousand, hundred
  FROM a5_oa a LEFT JOIN a5_ob b ON q1 = unique2
  WHERE coalesce(thousand, 123) = q1 AND q1 = coalesce(hundred, 123) ORDER BY 1, 2;
SELECT q1, q2, unique2 FROM a5_oa a LEFT JOIN a5_ob b ON q1 = unique2
  WHERE coalesce(thousand, 5) = q1 AND q2 = coalesce(hundred, 5) AND q1 = q2 ORDER BY 1, 2;
SELECT count(*) FROM a5_oa a LEFT JOIN a5_ob b ON q1 = unique2
  WHERE coalesce(thousand, 123) = coalesce(hundred, 123) AND coalesce(hundred, 123) = coalesce(ten, 123);
EXPLAIN (COSTS OFF)
SELECT q1, unique2, thousand, hundred
  FROM a5_oa a LEFT JOIN a5_ob b ON q1 = unique2
  WHERE coalesce(thousand, 123) = q1 AND q1 = coalesce(hundred, 123);
EXPLAIN (COSTS OFF)
SELECT * FROM a5_oa a LEFT JOIN a5_ob b ON q1 = unique2
  WHERE coalesce(thousand, 123) = coalesce(hundred, 123) AND coalesce(hundred, 123) = coalesce(ten, 5)
    AND coalesce(ten, 5) = q1;
-- (A qual strict for the nullable side: an inner join, its own classes.)
SELECT q1, unique2 FROM a5_oa a LEFT JOIN a5_ob b ON q1 = unique2
  WHERE coalesce(thousand, 123) = q1 AND q1 = coalesce(hundred, 123) AND b.ten = 3;
RESET enable_memoize;
DROP TABLE a5_oa, a5_ob;
