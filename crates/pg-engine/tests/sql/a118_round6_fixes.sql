-- Difftest round 6 fixes (agent 118).
-- A GroupAggregate shows each group's key as its first row in the sort's order.
CREATE TABLE g (id int, iv interval, n numeric, f float8);
INSERT INTO g VALUES (1,'-178000000 years', 1.0, 0), (2,'1 mon', 1.00, -0), (5,'30 days', 1, 0),
  (6,'1 day 00:00:00.000001', 2.5, 1), (7,'178000000 years', 2.50, -0), (8,'2147483647 days', 3, 2), (9,NULL, NULL, NULL);
SET enable_hashagg = off;
SELECT iv, count(*) FROM g GROUP BY iv ORDER BY iv;
SELECT n, count(*) FROM g GROUP BY n ORDER BY n;
SELECT f, count(*) FROM g GROUP BY f ORDER BY f;
RESET enable_hashagg;
-- A virtual generated column is computed when RETURNING or a CHECK reads it.
CREATE TABLE v2 (id int PRIMARY KEY, e2 bigint GENERATED ALWAYS AS (id * 1000000000));
INSERT INTO v2 VALUES (15) RETURNING *;
INSERT INTO v2 VALUES (1) RETURNING e2, id;
INSERT INTO v2 VALUES (16) RETURNING id;
UPDATE v2 SET id = 17 WHERE id = 16 RETURNING old.e2, new.id;
UPDATE v2 SET id = 3 WHERE id = 17 RETURNING old.e2, new.e2;
SELECT id FROM v2 ORDER BY id;
CREATE TABLE v3 (id int, e int GENERATED ALWAYS AS (id * 1000000000) VIRTUAL CHECK (e > 0));
INSERT INTO v3 VALUES (15);
INSERT INTO v3 VALUES (1);
SELECT * FROM v3;
-- A NOT NULL constraint added NOT VALID proves nothing to the planner.
CREATE TABLE r (id int PRIMARY KEY, doc text);
INSERT INTO r VALUES (16, NULL), (17, 'x');
ALTER TABLE r ADD CONSTRAINT nn NOT NULL doc NOT VALID;
SELECT id FROM r WHERE doc IS NULL;
SELECT count(*) FROM r WHERE doc IS NOT NULL;
SELECT r1.id FROM r r1 LEFT JOIN r r2 ON r2.id = r1.id + 1 WHERE r2.doc IS NULL ORDER BY 1;
-- Record comparisons look a column's operator up only as they reach it.
CREATE TABLE j1 (id int PRIMARY KEY, b int, e json);
INSERT INTO j1 VALUES (1, 5, '{}');
CREATE FUNCTION jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'jt'; RETURN NEW; END $$;
CREATE TRIGGER jtu AFTER UPDATE ON j1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION jt();
UPDATE j1 SET b = 1;
UPDATE j1 SET e = '[]';
SELECT * FROM j1;
SELECT x IS DISTINCT FROM y FROM j1 x, (SELECT 1, 6, '{}'::json) y;
SELECT x = y FROM j1 x, (SELECT 1, 1, '{}'::json) y;
SELECT ROW(1, 2) = ROW(1, 2, 3);
SELECT ROW(1, 2) = ROW(2, 2, 3);
SELECT ROW(1, 'a') < ROW(1, 2);
SELECT ROW(1, 'a') < ROW(2, 2);
SELECT ROW(NULL::int, 'a') = ROW(1, 2);
-- DISTINCT aggregates match ORDER BY sub-selects to their arguments as PostgreSQL's equal().
CREATE TABLE d1 (a int);
INSERT INTO d1 VALUES (1), (2);
SELECT array_agg(DISTINCT (SELECT d1.a + 1) ORDER BY (SELECT d1.a + 1)) FROM d1;
SELECT array_agg(DISTINCT (SELECT d1.a + 1) ORDER BY (SELECT d1.a + 2)) FROM d1;
-- Constant AND / OR (a BETWEEN) and IN lists fold, as eval_const_expressions has them.
CREATE TABLE f1 (a int);
CREATE TABLE f2 (b int);
INSERT INTO f1 VALUES (1);
INSERT INTO f2 VALUES (2);
SELECT * FROM f1 FULL JOIN f2 ON 1 IN (2, 3);
SELECT * FROM f1 FULL JOIN f2 ON 1 NOT IN (1, 3);
SELECT * FROM f1 FULL JOIN f2 ON CASE WHEN 1 BETWEEN 0 AND 2 THEN false ELSE f1.a > f2.b END;
SELECT * FROM f1 WHERE CASE WHEN 1 BETWEEN 0 AND 2 THEN true ELSE (SELECT 1/0) = 1 END;
SELECT * FROM f1 WHERE CASE WHEN 3 NOT BETWEEN SYMMETRIC 2 AND 4 THEN (SELECT 1/0) = 1 ELSE true END;
EXPLAIN (COSTS OFF) SELECT * FROM f1 WHERE a > 0 AND 1 BETWEEN 0 AND 2;
EXPLAIN (COSTS OFF) SELECT * FROM f1 WHERE a > 0 OR 2 IN (1, NULL);
