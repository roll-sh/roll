-- Round 16 (agent 20): a semi join unique-ified under FOR UPDATE locks the joined rows at
-- the level's top (its HashAggregate carries the inner relation's ctid through, ungrouped);
-- a LIMIT subquery's pushed-down lock returns its rows' ctids through the Limit; RETURNING's
-- sub-selects show without VERBOSE; an UPDATE's SET sub-selects and casts are costed per row
-- (the target list's cost_qual_eval / cost_subplan).
CREATE TABLE a20_t1 (a int, b int);
CREATE TABLE a20_t2 (x int, y int);
INSERT INTO a20_t1 SELECT i, i FROM generate_series(1, 100) i;
INSERT INTO a20_t2 SELECT i % 10, i FROM generate_series(1, 1000) i;
ANALYZE a20_t1;
ANALYZE a20_t2;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a20_t1 WHERE a IN (SELECT x FROM a20_t2) FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a20_t1 WHERE a IN (SELECT x FROM a20_t2) FOR UPDATE OF a20_t1;
SELECT * FROM a20_t1 WHERE a IN (SELECT x FROM a20_t2) ORDER BY a FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT * FROM a20_t1 LIMIT 2) s FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM (SELECT * FROM a20_t1 ORDER BY a DESC LIMIT 1) s FOR UPDATE;
SELECT * FROM (SELECT * FROM a20_t1 ORDER BY a DESC LIMIT 1) s FOR UPDATE;
-- RETURNING's sub-selects
CREATE TABLE a20_m AS SELECT i AS k, (i || ' v')::text v FROM generate_series(1, 16, 3) i;
CREATE TABLE a20_src AS SELECT i AS k, i AS w FROM generate_series(1, 1000) i;
ANALYZE a20_m;
ANALYZE a20_src;
EXPLAIN (COSTS OFF)
DELETE FROM a20_m RETURNING (SELECT max(w) FROM a20_src s2 WHERE s2.k = a20_m.k);
EXPLAIN (COSTS OFF)
INSERT INTO a20_m VALUES (1, 'a') RETURNING (SELECT max(w) FROM a20_src s2 WHERE s2.k = a20_m.k);
EXPLAIN (COSTS OFF)
UPDATE a20_m SET v = 'z' RETURNING (SELECT max(w) FROM a20_src s2 WHERE s2.k = a20_m.k);
-- SET's costs
EXPLAIN UPDATE a20_m SET v = (SELECT max(w)::text FROM a20_src s2 WHERE s2.k = a20_m.k);
EXPLAIN UPDATE a20_m SET v = (SELECT max(w)::text FROM a20_src s2 WHERE s2.k = a20_m.k)
WHERE k > 3;
EXPLAIN UPDATE a20_m SET v = k::text;
EXPLAIN UPDATE a20_m SET v = upper(v);
EXPLAIN UPDATE a20_m SET k = k + 1, v = v || 'x';
DROP TABLE a20_t1, a20_t2, a20_m, a20_src;
