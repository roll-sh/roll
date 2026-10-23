-- EXPLAIN of a statement a rule rewrote: each statement's plan apart, a blank line between
-- (an INSTEAD action, a qualified rule's original statement and action, a DO ALSO action);
-- the statement's relations named before the action's; NEW from a one-row VALUES put in
-- (the qualification an InitPlan); a security barrier view an UPDATE joins kept as its
-- Subquery Scan, a pseudoconstant qual inside it. A data-modifying statement's other
-- relations and a locking level's unlocked ones return their ctid: no index-only scan.
CREATE FUNCTION a9_snoop(anyelement) RETURNS boolean AS
$$ BEGIN RAISE NOTICE 'snooped value: %', $1; RETURN true; END; $$
LANGUAGE plpgsql COST 0.000001;
CREATE TABLE a9b (id int PRIMARY KEY, data text, deleted boolean);
INSERT INTO a9b VALUES (1, 'Row 1', false), (2, 'Row 2', true);
CREATE RULE a9b_ins AS ON INSERT TO a9b
  WHERE EXISTS (SELECT 1 FROM a9b t WHERE t.id = new.id)
  DO INSTEAD UPDATE a9b SET data = new.data, deleted = false WHERE id = new.id;
CREATE RULE a9b_del AS ON DELETE TO a9b
  DO INSTEAD UPDATE a9b SET deleted = true WHERE id = old.id;
CREATE VIEW a9bv WITH (security_barrier = true) AS SELECT id, data FROM a9b WHERE NOT deleted;
EXPLAIN (COSTS OFF) DELETE FROM a9bv WHERE id = 1 AND a9_snoop(data);
DELETE FROM a9bv WHERE id = 1 AND a9_snoop(data);
EXPLAIN (COSTS OFF) INSERT INTO a9bv VALUES (2, 'New row 2');
INSERT INTO a9bv VALUES (2, 'New row 2');
INSERT INTO a9bv VALUES (3, 'Row 3');
SELECT * FROM a9b ORDER BY id;
DROP TABLE a9b CASCADE;
CREATE TABLE a9t1 (a int);
CREATE VIEW a9v1 WITH (security_barrier = true) AS SELECT * FROM a9t1;
CREATE RULE a9v1_upd AS ON UPDATE TO a9v1 DO INSTEAD UPDATE a9t1 SET a = NEW.a WHERE a = OLD.a;
CREATE VIEW a9v2 WITH (security_barrier = true) AS SELECT * FROM a9v1 WHERE EXISTS (SELECT 1);
EXPLAIN (COSTS OFF) UPDATE a9v2 SET a = 1;
INSERT INTO a9t1 VALUES (5), (6);
UPDATE a9v2 SET a = 1;
SELECT * FROM a9t1 ORDER BY a;
DROP VIEW a9v2, a9v1;
DROP TABLE a9t1;
CREATE TABLE a9k1 (a integer PRIMARY KEY, b text);
CREATE TABLE a9k2 (a integer PRIMARY KEY, b integer REFERENCES a9k1);
CREATE RULE a9r1 AS ON DELETE TO a9k1 DO DELETE FROM a9k2 WHERE a9k2.b = old.a;
EXPLAIN (COSTS OFF) DELETE FROM a9k1 WHERE a = 1;
INSERT INTO a9k1 VALUES (1, 'x'), (2, 'y');
INSERT INTO a9k2 VALUES (10, 1), (20, 2);
DELETE FROM a9k1 WHERE a = 1;
SELECT * FROM a9k1 ORDER BY a;
SELECT * FROM a9k2 ORDER BY a;
DROP RULE a9r1 ON a9k1;
CREATE TABLE a9k3 (a int PRIMARY KEY, c int);
EXPLAIN (COSTS OFF) UPDATE a9k1 SET b = 'z' FROM a9k3 WHERE a9k3.a = a9k1.a AND a9k3.a = 3;
EXPLAIN (COSTS OFF) DELETE FROM a9k1 USING a9k3 WHERE a9k3.a = a9k1.a AND a9k1.a = 3;
EXPLAIN (COSTS OFF) SELECT a9k1.b FROM a9k1, a9k3 WHERE a9k3.a = a9k1.a AND a9k1.a = 3 FOR UPDATE OF a9k1;
EXPLAIN (COSTS OFF) SELECT a9k3.a FROM a9k3 WHERE a9k3.a = 3;
DROP TABLE a9k2, a9k1, a9k3;
DROP FUNCTION a9_snoop(anyelement);
