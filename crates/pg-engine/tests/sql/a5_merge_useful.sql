-- A left join's clause rebuilt from a hash key under a filter (COALESCE equated with a
-- constant above outer joins); a parameterized bitmap scan costed for its repeated scans;
-- a merge join's input order kept useful by an outer join's clause; an EXISTS over a
-- subquery planned with sorts pulled up.
CREATE TABLE a5_tk (unique1 int, unique2 int, thousand int, twothousand int);
INSERT INTO a5_tk SELECT (i * 7919) % 10000, i, ((i * 7919) % 10000) % 1000, ((i * 7919) % 10000) % 2000
  FROM generate_series(0, 9999) i;
CREATE INDEX a5_tk_unique2 ON a5_tk (unique2);
CREATE INDEX a5_tk_thous ON a5_tk (thousand);
VACUUM ANALYZE a5_tk;
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM a5_tk a LEFT JOIN a5_tk b ON b.thousand = a.unique1
  LEFT JOIN a5_tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 44;
SELECT a.unique1, b.unique1
  FROM a5_tk a LEFT JOIN a5_tk b ON b.thousand = a.unique1
  LEFT JOIN a5_tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 4;
EXPLAIN (COSTS OFF)
SELECT a.unique1, b.unique1, c.unique1, coalesce(b.twothousand, a.twothousand)
  FROM a5_tk a LEFT JOIN a5_tk b ON b.thousand = a.unique1
  LEFT JOIN a5_tk c ON c.unique2 = coalesce(b.twothousand, a.twothousand)
  WHERE a.unique2 < 10 AND coalesce(b.twothousand, a.twothousand) = 44;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN SELECT a.unique1, b.twothousand FROM a5_tk a LEFT JOIN a5_tk b ON b.thousand = a.unique1
  WHERE a.unique2 < 30;
RESET enable_hashjoin;
RESET enable_mergejoin;
CREATE TABLE a5_tt (c1 int, c2 int, c3 int);
EXPLAIN (COSTS OFF)
SELECT t5.c1 FROM (a5_tt t2 LEFT JOIN a5_tt t3 ON t2.c3 = t3.c1)
  LEFT JOIN (a5_tt t4 JOIN a5_tt t5 ON t4.c2 = t5.c1) ON t3.c2 = t5.c1;
SELECT * FROM a5_tt t1
WHERE NOT EXISTS (
  SELECT 1 FROM a5_tt t2
    LEFT JOIN a5_tt t3 ON t2.c3 = t3.c1
    LEFT JOIN (SELECT t5.c1 AS c1 FROM a5_tt t4 LEFT JOIN a5_tt t5 ON t4.c2 = t5.c1) a1 ON t3.c2 = a1.c1
  WHERE t1.c1 = t2.c2
);
EXPLAIN (COSTS OFF)
SELECT * FROM a5_tt t1
WHERE NOT EXISTS (
  SELECT 1 FROM a5_tt t2
    LEFT JOIN a5_tt t3 ON t2.c3 = t3.c1
    LEFT JOIN (SELECT t5.c1 AS c1 FROM a5_tt t4 LEFT JOIN a5_tt t5 ON t4.c2 = t5.c1) a1 ON t3.c2 = a1.c1
  WHERE t1.c1 = t2.c2
);
DROP TABLE a5_tk, a5_tt;
