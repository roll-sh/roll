-- Round 14 (agent 4), fourth batch: a join under a window planned for the window's order
-- (and, under a LIMIT, for the rows read before its first output), a Memoize over an
-- append relation's parameterized members, a filter over an outer join costed with it.
CREATE TABLE ga4_tk (unique1 int, unique2 int, thousand int, tenthous int, filler text);
INSERT INTO ga4_tk SELECT i, (i * 7919) % 10000, i % 1000, i, repeat('x', 100) FROM generate_series(0, 9999) i;
CREATE INDEX ga4_tk_u1 ON ga4_tk (unique1);
CREATE INDEX ga4_tk_thous_tenthous ON ga4_tk (thousand, tenthous);
VACUUM ANALYZE ga4_tk;
EXPLAIN (COSTS OFF)
SELECT COUNT(*) OVER (ORDER BY t1.unique1)
FROM ga4_tk t1 INNER JOIN ga4_tk t2 ON t1.unique1 = t2.tenthous
LIMIT 1;
EXPLAIN (COSTS OFF)
SELECT COUNT(*) OVER (ORDER BY t1.unique1 ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)
FROM ga4_tk t1 INNER JOIN ga4_tk t2 ON t1.unique1 = t2.tenthous
LIMIT 1;
EXPLAIN (COSTS OFF)
SELECT COUNT(*) OVER (ORDER BY t1.unique1 ROWS BETWEEN UNBOUNDED PRECEDING AND 10000 FOLLOWING)
FROM ga4_tk t1 INNER JOIN ga4_tk t2 ON t1.unique1 = t2.tenthous
LIMIT 1;
EXPLAIN (COSTS OFF)
SELECT sum(t2.unique2) OVER (PARTITION BY t1.thousand)
FROM ga4_tk t1 INNER JOIN ga4_tk t2 ON t1.unique1 = t2.unique2;

CREATE TABLE ga4_u1 (a int);
CREATE TABLE ga4_u2 (a int);
CREATE TABLE ga4_u3 (a int);
INSERT INTO ga4_u1 SELECT i FROM generate_series(1, 1000) i;
INSERT INTO ga4_u2 SELECT i FROM generate_series(1, 1000) i;
INSERT INTO ga4_u3 SELECT i % 50 FROM generate_series(1, 100) i;
CREATE INDEX ON ga4_u1 (a);
CREATE INDEX ON ga4_u2 (a);
ANALYZE ga4_u1;
ANALYZE ga4_u2;
ANALYZE ga4_u3;
EXPLAIN (COSTS OFF)
SELECT * FROM ga4_u3 t1 JOIN (SELECT * FROM ga4_u1 UNION ALL SELECT * FROM ga4_u2) t2 ON t1.a = t2.a;
SELECT count(*) FROM ga4_u3 t1 JOIN (SELECT * FROM ga4_u1 UNION ALL SELECT * FROM ga4_u2) t2 ON t1.a = t2.a;

CREATE TABLE ga4_g32 (
  a int PRIMARY KEY,
  b int GENERATED ALWAYS AS (a * 2),
  c int GENERATED ALWAYS AS (10 + 10),
  d int GENERATED ALWAYS AS (coalesce(a, 100)),
  e int
);
INSERT INTO ga4_g32 VALUES (1), (2);
ANALYZE ga4_g32;
EXPLAIN (COSTS OFF)
SELECT t1.a FROM ga4_g32 t1 LEFT JOIN ga4_g32 t2 ON t1.a = t2.a WHERE coalesce(t2.b, 1) = 2;
EXPLAIN (COSTS OFF)
SELECT t1.a FROM ga4_g32 t1 LEFT JOIN ga4_g32 t2 ON t1.a = t2.a WHERE coalesce(t2.b, 1) = 2 OR t1.a IS NULL;
SELECT t1.a FROM ga4_g32 t1 LEFT JOIN ga4_g32 t2 ON t1.a = t2.a WHERE coalesce(t2.b, 1) = 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT t2.* FROM ga4_g32 t1 LEFT JOIN ga4_g32 t2 ON false;
