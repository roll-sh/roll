-- An inner join's ON clauses taking part in equivalence classes (a constant reaching the
-- other side), at least one row per page in a never-analyzed table's estimate, an x = x
-- over an outer query's value left as written, and a dummy appendrel's columns.
CREATE TABLE gc1_m1 (hundred int, thousand int);
CREATE TABLE gc1_m2 (hundred int, thousand int, tenthous int);
INSERT INTO gc1_m1 SELECT g % 100, g % 1000 FROM generate_series(1, 10000) g;
INSERT INTO gc1_m2 SELECT g % 100, g % 1000, g FROM generate_series(1, 10000) g;
CREATE INDEX gc1_m1_h ON gc1_m1 (hundred);
CREATE INDEX gc1_m2_h ON gc1_m2 (hundred);
ANALYZE gc1_m1;
ANALYZE gc1_m2;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gc1_m1 JOIN gc1_m2 ON gc1_m1.hundred = 42 AND gc1_m2.hundred = gc1_m1.hundred;
SELECT count(*) FROM gc1_m1 JOIN gc1_m2 ON gc1_m1.hundred = 42 AND gc1_m2.hundred = gc1_m1.hundred;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gc1_m1 JOIN gc1_m2 ON
  gc1_m1.hundred = 42 AND (gc1_m2.thousand = 42 OR gc1_m2.thousand = 41 OR gc1_m2.tenthous = 2) AND
  gc1_m2.hundred = gc1_m1.hundred;

CREATE TABLE gc1_ff (n char(1000)) WITH (fillfactor = 10, autovacuum_enabled = off);
INSERT INTO gc1_ff SELECT 'x' FROM generate_series(1, 1000);
EXPLAIN SELECT * FROM gc1_ff;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, x FROM (SELECT (SELECT random() WHERE y = y) AS x FROM (VALUES (1), (2)) v(y)) ss;

CREATE TABLE gc1_pt (tid int PRIMARY KEY) PARTITION BY LIST (tid);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gc1_pt;
SELECT * FROM gc1_pt;

-- A join alias naming more columns than the join has.
SELECT * FROM (gc1_m1 i CROSS JOIN gc1_pt j) ss(a, b, c, d);
SELECT * FROM (gc1_m1 i CROSS JOIN gc1_pt j) ss(a, b, c);
