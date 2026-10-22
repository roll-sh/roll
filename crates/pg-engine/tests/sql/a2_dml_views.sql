-- Writes through views with INSTEAD OF triggers read the view's query as a subquery: one
-- Postgres doesn't pull up stays a Subquery Scan (the statement's quals above it, the
-- equivalence class's equality last), the result relation named after it; a bitmap heap
-- scan reads rows in their HOT chains' root order; a called function's clause is costed by
-- its procost and estimated at the default selectivity; EXISTS drops a constant select list.
CREATE TABLE a2_dv_base (a int PRIMARY KEY, b text DEFAULT 'Unspecified');
INSERT INTO a2_dv_base SELECT i, 'Row ' || i FROM generate_series(-2, 2) g(i);
CREATE VIEW a2_dv_view1 AS
  SELECT *, 'Const1' AS c1 FROM a2_dv_base WHERE a>0 OFFSET 0;
CREATE VIEW a2_dv_view2 AS
  SELECT *, 'Const2' AS c2 FROM a2_dv_view1 WHERE a<10;
CREATE FUNCTION a2_dv_trig_fn() RETURNS trigger AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO a2_dv_base VALUES (NEW.a, NEW.b);
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    UPDATE a2_dv_base SET b=NEW.b WHERE a=OLD.a;
    RETURN NEW;
  ELSIF TG_OP = 'DELETE' THEN
    DELETE FROM a2_dv_base WHERE a=OLD.a;
    RETURN OLD;
  END IF;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER a2_dv_view1_ins_trig INSTEAD OF INSERT ON a2_dv_view1
  FOR EACH ROW EXECUTE PROCEDURE a2_dv_trig_fn();
CREATE TRIGGER a2_dv_view1_upd_trig INSTEAD OF UPDATE ON a2_dv_view1
  FOR EACH ROW EXECUTE PROCEDURE a2_dv_trig_fn();
CREATE TRIGGER a2_dv_view1_del_trig INSTEAD OF DELETE ON a2_dv_view1
  FOR EACH ROW EXECUTE PROCEDURE a2_dv_trig_fn();
EXPLAIN (costs off) UPDATE a2_dv_view2 SET a=3 WHERE a=2;
EXPLAIN (costs off) DELETE FROM a2_dv_view2 WHERE a=2;
EXPLAIN (costs off)
MERGE INTO a2_dv_view2 t
  USING (SELECT x, 'R'||x FROM generate_series(0,3) x) AS s(a,b) ON t.a = s.a
  WHEN MATCHED AND t.a <= 1 THEN DELETE
  WHEN MATCHED THEN UPDATE SET b = s.b
  WHEN NOT MATCHED AND s.a > 0 THEN INSERT VALUES (s.a, s.b);
EXPLAIN (costs off) UPDATE a2_dv_view1 SET b = 'x' WHERE a = 1;
CREATE VIEW a2_dv_view3 AS SELECT * FROM a2_dv_base WHERE a > 0;
CREATE TRIGGER a2_dv_view3_upd_trig INSTEAD OF UPDATE ON a2_dv_view3
  FOR EACH ROW EXECUTE PROCEDURE a2_dv_trig_fn();
EXPLAIN (costs off) UPDATE a2_dv_view3 SET b = 'x' WHERE a = 1;
UPDATE a2_dv_view2 SET b = 'y' WHERE a = 1 RETURNING *;
DROP TABLE a2_dv_base CASCADE;
DROP FUNCTION a2_dv_trig_fn();


CREATE TABLE a2_dv_base (a int PRIMARY KEY, b text DEFAULT 'Unspecified');
INSERT INTO a2_dv_base SELECT i, 'Row ' || i FROM generate_series(-2, 2) g(i);

CREATE VIEW a2_dv_v1 AS
  SELECT *, 'Const' AS c, (SELECT concat('b: ', b)) AS d FROM a2_dv_base WHERE a>0;

INSERT INTO a2_dv_v1 VALUES (3, 'Row 3');
INSERT INTO a2_dv_v1 (a) VALUES (4);
UPDATE a2_dv_v1 SET a=5 WHERE a=4;
DELETE FROM a2_dv_v1 WHERE b='Row 2';

SET jit_above_cost = 0;

MERGE INTO a2_dv_v1 t
  USING (VALUES (0, 'ROW 0'), (1, 'ROW 1'),
                (2, 'ROW 2'), (3, 'ROW 3')) AS v(a,b) ON t.a = v.a
  WHEN MATCHED AND t.a <= 1 THEN UPDATE SET b = v.b
  WHEN MATCHED THEN DELETE
  WHEN NOT MATCHED AND a > 0 THEN INSERT (a) VALUES (v.a)
  RETURNING merge_action(), v.*, old.*, new.*, t.*;

SET jit_above_cost TO DEFAULT;


MERGE INTO a2_dv_v1 t
  USING (VALUES (0, 'R0'), (1, 'R1'),
                (2, 'R2'), (3, 'R3')) AS v(a,b) ON t.a = v.a
  WHEN MATCHED AND t.a <= 1 THEN UPDATE SET b = v.b
  WHEN MATCHED THEN DELETE
  WHEN NOT MATCHED BY SOURCE THEN DELETE
  WHEN NOT MATCHED AND a > 0 THEN INSERT (a) VALUES (v.a)
  RETURNING merge_action(), v.*, t.*;
EXPLAIN (COSTS OFF) MERGE INTO a2_dv_v1 t
  USING (VALUES (0, 'R0'), (1, 'R1'),
                (2, 'R2'), (3, 'R3')) AS v(a,b) ON t.a = v.a
  WHEN MATCHED AND t.a <= 1 THEN UPDATE SET b = v.b
  WHEN MATCHED THEN DELETE
  WHEN NOT MATCHED BY SOURCE THEN DELETE
  WHEN NOT MATCHED AND a > 0 THEN INSERT (a) VALUES (v.a);
DROP TABLE a2_dv_base CASCADE;

CREATE TABLE a2_dv_base (person text, visibility text);
INSERT INTO a2_dv_base VALUES ('Tom', 'public'),
                            ('Dick', 'private'),
                            ('Harry', 'public');

CREATE VIEW rw_view1 AS
  SELECT person FROM a2_dv_base WHERE visibility = 'public';

CREATE FUNCTION a2_dv_snoop(anyelement)
RETURNS boolean AS
$$
BEGIN
  RAISE NOTICE 'a2_dv_snooped value: %', $1;
  RETURN true;
END;
$$
LANGUAGE plpgsql COST 0.000001;
EXPLAIN SELECT * FROM a2_dv_base WHERE a2_dv_snoop(person);
EXPLAIN SELECT * FROM a2_dv_base WHERE visibility = 'public';
EXPLAIN SELECT * FROM a2_dv_base WHERE a2_dv_snoop(person) AND visibility = 'public';
DROP TABLE a2_dv_base CASCADE; DROP FUNCTION a2_dv_snoop(anyelement);

CREATE TABLE a2_dv_p (a int);
CREATE TABLE a2_dv_c (b int) INHERITS (a2_dv_p);
INSERT INTO a2_dv_p SELECT g FROM generate_series(1, 5) g;
INSERT INTO a2_dv_c SELECT g, g FROM generate_series(1, 5) g;
CREATE TABLE a2_dv_o (a int);
INSERT INTO a2_dv_o VALUES (1), (7);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a2_dv_o o WHERE EXISTS (SELECT 1 FROM a2_dv_p p WHERE p.a > o.a + 3) OR o.a > 6;
SELECT * FROM a2_dv_o o WHERE EXISTS (SELECT 1 FROM a2_dv_p p WHERE p.a > o.a + 3) OR o.a > 6;
DROP TABLE a2_dv_p, a2_dv_c, a2_dv_o CASCADE;

-- (WITH CHECK OPTION's sub-selects are the ModifyTable's SubPlans; the result relation has
-- the view's alias for it.)
CREATE TABLE a2_dv_wb (a int);
CREATE TABLE a2_dv_wr (a int PRIMARY KEY);
INSERT INTO a2_dv_wr SELECT * FROM generate_series(1, 10);
CREATE VIEW a2_dv_wv AS
  SELECT * FROM a2_dv_wb b WHERE EXISTS (SELECT 1 FROM a2_dv_wr r WHERE r.a = b.a)
  WITH CHECK OPTION;
INSERT INTO a2_dv_wv VALUES (5);
INSERT INTO a2_dv_wv VALUES (15);
EXPLAIN (COSTS OFF) INSERT INTO a2_dv_wv VALUES (5);
DROP TABLE a2_dv_wb, a2_dv_wr CASCADE;
