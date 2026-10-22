-- Index conditions under nondeterministic collations (match_pattern_prefix: an exact
-- pattern as `=` under the index's collation; an equality under the column's own
-- collation, which the byte-ordered index can't seek: the scan checks it on each row, the
-- rows in heap order); a DISTINCT ON subquery takes a qual under its key's own collation,
-- the key then fixed (a Limit, its sort key gone).
CREATE COLLATION a2_ci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE TABLE a2_t1ci (x text COLLATE a2_ci);
INSERT INTO a2_t1ci VALUES ('abc'), ('ABC'), ('def'), ('ghi');
CREATE INDEX ON a2_t1ci (x);
SET enable_seqscan = off;
SET enable_indexonlyscan = off;
EXPLAIN (COSTS OFF) SELECT * FROM a2_t1ci WHERE x = 'abc';
SELECT * FROM a2_t1ci WHERE x = 'abc';
EXPLAIN (COSTS OFF) SELECT * FROM a2_t1ci WHERE x ~ '^abc$' COLLATE "C";
SELECT * FROM a2_t1ci WHERE x ~ '^abc$' COLLATE "C";
EXPLAIN (COSTS OFF) SELECT * FROM a2_t1ci WHERE x LIKE 'abc' COLLATE a2_ci;
SELECT * FROM a2_t1ci WHERE x LIKE 'abc' COLLATE a2_ci;
RESET enable_seqscan; RESET enable_indexonlyscan;
DROP TABLE a2_t1ci; DROP COLLATION a2_ci;

CREATE COLLATION a2_ci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE TABLE a2_pci (id int, x text COLLATE a2_ci);
INSERT INTO a2_pci VALUES (1, 'ABC'), (2, 'abc'), (3, 'def');
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT DISTINCT ON (x) id, x FROM a2_pci ORDER BY x, id) s
WHERE x = 'abc' COLLATE a2_ci;
SELECT * FROM (SELECT DISTINCT ON (x) id, x FROM a2_pci ORDER BY x, id) s
WHERE x = 'abc' COLLATE a2_ci;
DROP TABLE a2_pci; DROP COLLATION a2_ci;

CREATE COLLATION a2_ci2 (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE TABLE a2_t2ci (x text COLLATE a2_ci2, y int);
INSERT INTO a2_t2ci SELECT CASE WHEN g % 2 = 0 THEN 'abc' ELSE 'ABC' END || (g % 50), g FROM generate_series(1, 2000) g;
CREATE INDEX ON a2_t2ci (x);
ANALYZE a2_t2ci;
EXPLAIN (COSTS OFF) SELECT * FROM a2_t2ci WHERE x = 'abc7';
SELECT count(*), min(y), max(y) FROM a2_t2ci WHERE x = 'abc7';
SELECT y FROM a2_t2ci WHERE x = 'ABC7' ORDER BY y LIMIT 3;
EXPLAIN (COSTS OFF) SELECT * FROM a2_t2ci WHERE x LIKE 'abc1%';
DROP TABLE a2_t2ci; DROP COLLATION a2_ci2;

-- (An ORDER BY key under an explicit COLLATE is the sort's collation; a join clause between
-- columns of the class's collation shows no relabelling.)
CREATE COLLATION a2_ci3 (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE TABLE a2_pt3 (a text, c text COLLATE a2_ci3) PARTITION BY LIST (c COLLATE "C");
CREATE TABLE a2_pt3_p1 PARTITION OF a2_pt3 FOR VALUES IN ('a', 'b');
CREATE TABLE a2_pt3_p2 PARTITION OF a2_pt3 FOR VALUES IN ('B', 'A');
INSERT INTO a2_pt3 SELECT i % 4 + 1, substr('abAB', (i % 4) + 1, 1) FROM generate_series(0, 19) i;
ANALYZE a2_pt3;
EXPLAIN (COSTS OFF)
SELECT t1.c, count(t2.c) FROM a2_pt3 t1 JOIN a2_pt3 t2 ON t1.c = t2.c GROUP BY 1 ORDER BY t1.c COLLATE "C";
SELECT t1.c, count(t2.c) FROM a2_pt3 t1 JOIN a2_pt3 t2 ON t1.c = t2.c GROUP BY 1 ORDER BY t1.c COLLATE "C";
DROP TABLE a2_pt3; DROP COLLATION a2_ci3;
