CREATE TABLE ip (a int, b text DEFAULT 'pdef');
CREATE TABLE ic1 (c int, b text DEFAULT 'c1def') INHERITS (ip);
CREATE TABLE ic2 (d text, a int) INHERITS (ip);
INSERT INTO ip VALUES (1, 'p1'), (2, 'p2');
INSERT INTO ic1 (a, b, c) VALUES (3, 'c1a', 30), (4, 'c1b', 40);
INSERT INTO ic2 (a, b, d) VALUES (5, 'c2a', 'x'), (6, 'c2b', 'y');
CREATE FUNCTION itrg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_LEVEL = 'ROW' THEN
    RAISE NOTICE '% % % on %: % -> %', TG_WHEN, TG_OP, TG_LEVEL, TG_TABLE_NAME, OLD, NEW;
  ELSE
    RAISE NOTICE '% % % on %', TG_WHEN, TG_OP, TG_LEVEL, TG_TABLE_NAME;
  END IF;
  RETURN COALESCE(NEW, OLD);
END $$;
CREATE TRIGGER bs BEFORE UPDATE OR DELETE ON ip FOR EACH STATEMENT EXECUTE FUNCTION itrg();
CREATE TRIGGER zs AFTER UPDATE OR DELETE ON ip FOR EACH STATEMENT EXECUTE FUNCTION itrg();
CREATE TRIGGER br BEFORE UPDATE OR DELETE ON ip FOR EACH ROW EXECUTE FUNCTION itrg();
CREATE TRIGGER zr AFTER UPDATE OR DELETE ON ip FOR EACH ROW EXECUTE FUNCTION itrg();
CREATE TRIGGER br BEFORE UPDATE OR DELETE ON ic1 FOR EACH ROW EXECUTE FUNCTION itrg();
CREATE TRIGGER zr AFTER UPDATE OR DELETE ON ic2 FOR EACH ROW EXECUTE FUNCTION itrg();
CREATE TRIGGER cs AFTER UPDATE OR DELETE ON ic1 FOR EACH STATEMENT EXECUTE FUNCTION itrg();
-- One statement over every member: the named table's statement triggers, each member's row
-- triggers, the AFTER ROW events in the order the rows changed.
UPDATE ip SET b = b || '!' WHERE a % 2 = 0 RETURNING *, tableoid::regclass;
UPDATE ip SET b = b || '?' WHERE a IN (1, 3, 5) RETURNING ip.*, tableoid::regclass;
-- DEFAULT is the named table's default.
UPDATE ip SET b = DEFAULT WHERE a IN (3, 5) RETURNING *, tableoid::regclass;
DELETE FROM ip WHERE a IN (2, 4, 6) RETURNING *, tableoid::regclass;
UPDATE ONLY ip SET b = 'only' RETURNING *;
SELECT tableoid::regclass, * FROM ip ORDER BY a;
SELECT * FROM ic1 ORDER BY a;
SELECT * FROM ic2 ORDER BY a;
DROP TRIGGER br ON ip;
DROP TRIGGER zr ON ip;
DROP TRIGGER br ON ic1;
DROP TRIGGER zr ON ic2;
-- Transition tables of the named table hold every member's rows, as its rows.
CREATE FUNCTION itrans() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    FOR r IN SELECT * FROM oldt ORDER BY a LOOP RAISE NOTICE '% old: %', TG_OP, r; END LOOP;
  END IF;
  IF TG_OP = 'UPDATE' THEN
    FOR r IN SELECT * FROM newt ORDER BY a LOOP RAISE NOTICE '% new: %', TG_OP, r; END LOOP;
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER tu AFTER UPDATE ON ip REFERENCING OLD TABLE AS oldt NEW TABLE AS newt FOR EACH STATEMENT EXECUTE FUNCTION itrans();
CREATE TRIGGER td AFTER DELETE ON ip REFERENCING OLD TABLE AS oldt FOR EACH STATEMENT EXECUTE FUNCTION itrans();
UPDATE ip SET a = a + 10;
DELETE FROM ip WHERE a > 14;
INSERT INTO ip VALUES (1, 'p1'), (2, 'p2');
INSERT INTO ic1 (a, b, c) VALUES (3, 'c1a', 30), (4, 'c1b', 40);
INSERT INTO ic2 (a, b, d) VALUES (5, 'c2a', 'x'), (6, 'c2b', 'y');
DROP TRIGGER tu ON ip;
DROP TRIGGER td ON ip;
DROP TRIGGER bs ON ip;
DROP TRIGGER zs ON ip;
DROP TRIGGER cs ON ic1;
-- Joined to other relations.
CREATE TABLE src (k int, v text);
INSERT INTO src VALUES (5, 'five'), (1, 'one'), (4, 'four'), (4, 'four again'), (99, 'none');
WITH u AS (UPDATE ip SET b = src.v FROM src WHERE ip.a = src.k RETURNING ip.a, ip.b, tableoid::regclass AS t) SELECT * FROM u ORDER BY a;
WITH d AS (DELETE FROM ip USING src WHERE ip.a = src.k AND src.v LIKE 'f%' RETURNING ip.*, src.v) SELECT * FROM d ORDER BY a;
SELECT tableoid::regclass, * FROM ip ORDER BY a;
-- Generated columns of a member are its own.
CREATE TABLE gp (a int, b int);
CREATE TABLE gc (g int GENERATED ALWAYS AS (a * 2) STORED) INHERITS (gp);
CREATE TABLE gc2 (g int GENERATED ALWAYS AS (a * 3) STORED) INHERITS (gp);
INSERT INTO gc (a, b) VALUES (1, 1), (2, 2);
INSERT INTO gc2 (a, b) VALUES (10, 10);
UPDATE gp SET a = a + 1 RETURNING *, tableoid::regclass;
SELECT * FROM gc ORDER BY a;
SELECT * FROM gc2 ORDER BY a;
-- Check constraints and NOT NULL of each member.
ALTER TABLE ic1 ADD CONSTRAINT ic1_a CHECK (a < 50);
UPDATE ip SET a = a + 50 WHERE a > 2;
UPDATE ip SET a = a + 50 WHERE a = 2 RETURNING *;
-- A view's check option over the parent checks each member's rows.
CREATE VIEW ipv AS SELECT * FROM ip WHERE a < 20 WITH CHECK OPTION;
UPDATE ipv SET a = a + 100 WHERE a = 3;
UPDATE ipv SET a = a + 1 WHERE a = 3 RETURNING *;
DELETE FROM ipv WHERE a = 4 RETURNING *;
SELECT tableoid::regclass, * FROM ip ORDER BY a;
-- WHERE CURRENT OF reaches the member the cursor's row is in.
BEGIN;
DECLARE cur CURSOR FOR SELECT * FROM ip WHERE a = 5 FOR UPDATE;
FETCH cur;
UPDATE ip SET b = 'cursor' WHERE CURRENT OF cur RETURNING *, tableoid::regclass;
DELETE FROM ip WHERE CURRENT OF cur RETURNING *;
COMMIT;
SELECT tableoid::regclass, * FROM ip ORDER BY a;
-- Row-level security: the named table's policies apply to every member's rows.
CREATE ROLE regress_gxid_rls;
GRANT ALL ON ip TO regress_gxid_rls;
ALTER TABLE ip ENABLE ROW LEVEL SECURITY;
CREATE POLICY ip_sel ON ip FOR SELECT USING (true);
CREATE POLICY ip_upd ON ip FOR UPDATE USING (a % 2 = 0) WITH CHECK (b <> 'bad');
CREATE POLICY ip_del ON ip FOR DELETE USING (a > 3);
SET ROLE regress_gxid_rls;
UPDATE ip SET b = b || '+' RETURNING *, tableoid::regclass;
UPDATE ip SET b = 'bad' WHERE a = 6;
DELETE FROM ip RETURNING *;
RESET ROLE;
DROP POLICY ip_sel ON ip;
DROP POLICY ip_upd ON ip;
DROP POLICY ip_del ON ip;
ALTER TABLE ip DISABLE ROW LEVEL SECURITY;
REVOKE ALL ON ip FROM regress_gxid_rls;
DROP ROLE regress_gxid_rls;
SELECT tableoid::regclass, * FROM ip ORDER BY a;
-- The plan: one ModifyTable naming each result relation, over the members' Append.
INSERT INTO ip VALUES (11, 'p11');
INSERT INTO ic1 (a, b) VALUES (13, 'c13');
INSERT INTO ic2 (a, b) VALUES (15, 'c15');
EXPLAIN (COSTS OFF) UPDATE ip SET b = 'x' WHERE a = 13;
EXPLAIN (COSTS OFF) DELETE FROM ip WHERE a = 13;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE ip SET b = b || 'x' WHERE a > 10;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE ip SET b = 'x' WHERE a > 10 RETURNING *;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM ip WHERE a > 10 RETURNING *, tableoid;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE ip SET b = 'x' WHERE false;
EXPLAIN (COSTS OFF) UPDATE ONLY ip SET b = 'x';
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) UPDATE ip SET b = b WHERE a > 10;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) DELETE FROM ip WHERE a > 14;
CREATE TABLE ck1 (CHECK (a > 100)) INHERITS (ip);
EXPLAIN (COSTS OFF) UPDATE ip SET b = 'x' WHERE a = 13;
EXPLAIN (COSTS OFF) UPDATE ip SET b = 'x' WHERE a = 113;
SELECT tableoid::regclass, * FROM ip ORDER BY a;
