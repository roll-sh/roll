-- An initplan inside a correlated sub-select that reads only the outer query's row (agent
-- 143): evaluated once per run of the sub-select, as ExecSetParamPlan does when its
-- parameter changes, not again for every row of the sub-select's scan.
CREATE TABLE a143_c (id int PRIMARY KEY);
CREATE TABLE a143_o (id int PRIMARY KEY, cust int);
CREATE TABLE a143_d (order_id int, n int);
INSERT INTO a143_c SELECT g FROM generate_series(1, 4) g;
INSERT INTO a143_o SELECT g, 1 + g % 4 FROM generate_series(1, 40) g;
INSERT INTO a143_d SELECT 1 + g % 40, g FROM generate_series(1, 400) g;
ANALYZE a143_c, a143_o, a143_d;
CREATE FUNCTION a143_note(int) RETURNS int LANGUAGE plpgsql AS
  $$ BEGIN RAISE NOTICE 'initplan %', $1; RETURN $1; END $$;
EXPLAIN (COSTS OFF)
SELECT c.id, (SELECT sum(d.n) FROM a143_d d
              WHERE d.order_id = (SELECT a143_note(o1.id) FROM a143_o o1 WHERE o1.cust = c.id ORDER BY o1.id LIMIT 1))
FROM a143_c c ORDER BY c.id;
SELECT c.id, (SELECT sum(d.n) FROM a143_d d
              WHERE d.order_id = (SELECT a143_note(o1.id) FROM a143_o o1 WHERE o1.cust = c.id ORDER BY o1.id LIMIT 1))
FROM a143_c c ORDER BY c.id;
-- two levels: the initplan reads both enclosing rows
SELECT c.id, (SELECT count(*) FROM a143_o o2 WHERE o2.cust = c.id AND
                (SELECT count(*) FROM a143_d d WHERE d.order_id = (SELECT max(o1.id) FROM a143_o o1 WHERE o1.cust = c.id AND o1.id <= o2.id)) > 0)
FROM a143_c c ORDER BY c.id;
-- under a lateral join the outer row changes per row
SELECT c.id, x.s FROM a143_c c, LATERAL (SELECT sum(d.n) AS s FROM a143_d d
  WHERE d.order_id = (SELECT min(o1.id) FROM a143_o o1 WHERE o1.cust = c.id)) x ORDER BY c.id;
DROP FUNCTION a143_note(int);
DROP TABLE a143_c, a143_o, a143_d;
