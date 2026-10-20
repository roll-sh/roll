-- Planner and EXPLAIN shapes (group C1), part 6: Merge Append over append relations,
-- set operation subquery scans, SRF output names, correlated MIN / MAX InitPlans,
-- SQL functions in FROM, volatile quals in joins, constraint exclusion of a named
-- partition, assignment expressions, RETURNING aliases, cross-type hash joins.
CREATE TABLE g6_ev (id int PRIMARY KEY, v text);
CREATE TABLE g6_ev_child () INHERITS (g6_ev);
CREATE TABLE g6_other (id int PRIMARY KEY, v text);
INSERT INTO g6_ev SELECT i, 'e' || i FROM generate_series(1, 20) i;
INSERT INTO g6_ev_child SELECT i, 'c' || i FROM generate_series(21, 30) i;
INSERT INTO g6_other SELECT i, 'o' || i FROM generate_series(5, 15) i;
ANALYZE g6_ev, g6_ev_child, g6_other;
EXPLAIN (COSTS OFF)
SELECT id FROM (SELECT id FROM g6_ev UNION ALL SELECT id FROM g6_other) ss ORDER BY id;
SELECT id FROM (SELECT id FROM g6_ev UNION ALL SELECT id FROM g6_other) ss ORDER BY id LIMIT 6;
EXPLAIN (COSTS OFF)
SELECT id FROM g6_ev UNION ALL SELECT id FROM g6_other ORDER BY id;
EXPLAIN (COSTS OFF)
SELECT id FROM (SELECT id FROM g6_ev UNION ALL SELECT id FROM g6_other) ss;
EXPLAIN (COSTS OFF) SELECT id FROM g6_ev ORDER BY id;
-- A filter a UNION can't take: its Subquery Scan, by the subquery's alias.
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT 1 AS t, generate_series(1, 10) AS x UNION SELECT 2 AS t, 4 AS x) ss
WHERE x < 4 ORDER BY x;
SELECT * FROM (SELECT 1 AS t, generate_series(1, 10) AS x UNION SELECT 2 AS t, 4 AS x) ss
WHERE x < 4 ORDER BY x;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1, generate_series(1, 3) + 1 AS x;
-- MIN / MAX in a correlated subquery: the InitPlan is redone for each outer value.
CREATE TABLE g6_n (n int);
INSERT INTO g6_n VALUES (0), (5), (17), (40);
EXPLAIN (COSTS OFF)
SELECT n, (SELECT min(id) FROM g6_ev WHERE id > n) FROM g6_n;
SELECT n, (SELECT min(id) FROM g6_ev WHERE id > n), (SELECT max(id) FROM g6_other WHERE id < n) FROM g6_n ORDER BY n;
EXPLAIN (COSTS OFF) SELECT DISTINCT min(id), max(id) FROM g6_ev;
-- A partial index's predicate implies the qual: no Index Cond for it.
CREATE TABLE g6_p (f int);
CREATE INDEX g6_p_i ON g6_p (f) WHERE f IS NOT NULL;
INSERT INTO g6_p SELECT i FROM generate_series(1, 1000) i;
ANALYZE g6_p;
EXPLAIN (COSTS OFF) SELECT min(f) FROM g6_p;
SELECT min(f), max(f) FROM g6_p;
-- Volatile quals go to the relation they read.
CREATE FUNCTION g6_leak(text) RETURNS bool LANGUAGE plpgsql AS
  'BEGIN RAISE NOTICE ''g6_leak => %'', $1; RETURN true; END';
EXPLAIN (COSTS OFF)
SELECT * FROM g6_ev e, g6_other o WHERE e.id = 3 AND o.id = 7 AND g6_leak(e.v) AND g6_leak(o.v);
SELECT * FROM g6_ev e, g6_other o WHERE e.id = 3 AND o.id = 7 AND g6_leak(e.v) AND g6_leak(o.v);
-- SQL functions in FROM: not SETOF, a Function Scan (its call folded or inlined).
CREATE TYPE g6_ty AS (f1 numeric(35,6), f2 numeric(35,2));
CREATE FUNCTION g6_imm() RETURNS g6_ty AS 'SELECT 7.136178319899999964, 7.136178319899999964'
  LANGUAGE sql IMMUTABLE;
CREATE FUNCTION g6_vol() RETURNS g6_ty AS 'SELECT 7.136178319899999964, 7.136178319899999964'
  LANGUAGE sql VOLATILE;
CREATE FUNCTION g6_five() RETURNS int AS 'SELECT 5' LANGUAGE sql VOLATILE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT g6_imm();
SELECT g6_imm();
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM g6_imm();
SELECT * FROM g6_imm();
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM g6_vol();
SELECT * FROM g6_vol() t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM g6_five();
SELECT * FROM g6_five();
-- constraint_exclusion = on: a partition named directly, by its partition constraint.
CREATE TABLE g6_lp (a int) PARTITION BY LIST (a);
CREATE TABLE g6_lp1 PARTITION OF g6_lp FOR VALUES IN (1);
SET constraint_exclusion = on;
EXPLAIN (COSTS OFF) SELECT * FROM g6_lp1 WHERE a = 2;
EXPLAIN (COSTS OFF) UPDATE g6_lp1 SET a = 1 WHERE a = 2;
RESET constraint_exclusion;
EXPLAIN (COSTS OFF) SELECT * FROM g6_lp1 WHERE a = 2;
-- Assignments into an array of composites.
CREATE TYPE g6_c AS (r float8, i float8);
CREATE TABLE g6_ct (d1 g6_c[]);
INSERT INTO g6_ct VALUES (ARRAY[ROW(1, 2)::g6_c]);
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE g6_ct SET d1[1].r = d1[1].r - 1, d1[1].i = d1[1].i + 1 WHERE d1[1].i > 0;
UPDATE g6_ct SET d1[1].r = d1[1].r - 1, d1[1].i = d1[1].i + 1 WHERE d1[1].i > 0 RETURNING *;
-- RETURNING's OLD / NEW under other names.
CREATE TABLE g6_r (a int, b text);
INSERT INTO g6_r VALUES (1, 'x');
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE g6_r SET b = 'y' RETURNING WITH (OLD AS o, NEW AS n) o.b, n.b, o.*;
UPDATE g6_r SET b = 'y' RETURNING WITH (OLD AS o, NEW AS n) o.b, n.b, o.*;
-- int4 = int8 joins hash (integer_ops).
CREATE TABLE g6_i4 (a int4);
CREATE TABLE g6_i8 (b int8);
INSERT INTO g6_i4 SELECT i FROM generate_series(1, 200) i;
INSERT INTO g6_i8 SELECT i * 2 FROM generate_series(1, 200) i;
ANALYZE g6_i4, g6_i8;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g6_i4 JOIN g6_i8 ON a = b;
SELECT count(*) FROM g6_i4 JOIN g6_i8 ON a = b;
-- An outer join under a qual on a PlaceHolderVar's contents is an inner join.
CREATE TABLE g6_a (id int, b1 bool);
CREATE TABLE g6_b (id int);
INSERT INTO g6_a VALUES (1, true), (2, false), (3, true);
INSERT INTO g6_b VALUES (1), (2);
SELECT a.id FROM g6_a a LEFT JOIN
  (SELECT g6_b.*, (g6_b.id IS NOT NULL) AS nn FROM g6_b) s ON s.id = a.id
WHERE a.b1 AND s.nn ORDER BY 1;
