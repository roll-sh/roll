-- Rule actions over a view: the view expanded after rewriteRuleAction (its relations
-- after the action's own in the range table, OLD last in the action's FROM list), the
-- view's NEW values computed where the action's SET reads them; inheritance parents'
-- tableoids in the order expand_inherited_rtentry adds them; the 10-page minimum skipped
-- for a table with relhassubclass set.
CREATE TABLE a29_foo (f1 int, f2 text, f3 int, f4 int8 DEFAULT 99);
CREATE TABLE a29_foochild (fc int) INHERITS (a29_foo);
DROP TABLE a29_foochild;
INSERT INTO a29_foo VALUES (1, 'test', 42), (2, 'more', 11), (3, 'zoo2', 57), (4, 'other', 7);
CREATE TABLE a29_joinme (f2j text, other int);
INSERT INTO a29_joinme VALUES ('more', 12345), ('zoo2', 54321), ('other', 0);
CREATE TABLE a29_plain (a int, b text);
INSERT INTO a29_plain VALUES (1, 'more'), (2, 'zoo2');
VACUUM ANALYZE a29_foo, a29_joinme, a29_plain;
-- (relhassubclass stays set after the last child is dropped: no 10-page minimum.)
CREATE TABLE a29_parent (a int, b text);
CREATE TABLE a29_kid () INHERITS (a29_parent);
DROP TABLE a29_kid;
INSERT INTO a29_parent VALUES (1, 'x'), (2, 'y');
EXPLAIN SELECT * FROM a29_parent;
CREATE TABLE a29_lone (a int, b text);
INSERT INTO a29_lone VALUES (1, 'x'), (2, 'y');
EXPLAIN SELECT * FROM a29_lone;

CREATE VIEW a29_joinview AS
  SELECT a29_foo.*, other FROM a29_foo JOIN a29_joinme ON (f2 = f2j);
CREATE RULE a29_joinview_u AS ON UPDATE TO a29_joinview DO INSTEAD
  UPDATE a29_foo SET f1 = new.f1, f3 = new.f3
    FROM a29_joinme WHERE f2 = f2j AND f2 = old.f2
    RETURNING a29_foo.*, other;
EXPLAIN (verbose, costs off)
UPDATE a29_joinview SET f3 = f3 + 1 WHERE f3 = 57
  RETURNING old.*, new.*, *, new.f3 - old.f3 AS delta_f3;
UPDATE a29_joinview SET f3 = f3 + 1 WHERE f3 = 57
  RETURNING old.*, new.*, *, new.f3 - old.f3 AS delta_f3;
EXPLAIN (verbose, costs off)
UPDATE a29_joinview SET f1 = f1 * 10, f3 = f3 + 1 WHERE other > 100 RETURNING *;
UPDATE a29_joinview SET f1 = f1 * 10, f3 = f3 + 1 WHERE other > 100 RETURNING *;
SELECT * FROM a29_foo ORDER BY f1;

-- The same query written by hand: a view's computed output in the SET is computed by
-- the join.
EXPLAIN (verbose, costs off)
UPDATE a29_foo SET f3 = v.x FROM (SELECT a29_foo.f3 + 1 AS x, a29_foo.f2
  FROM a29_foo JOIN a29_joinme ON f2 = f2j) v WHERE a29_foo.f2 = v.f2;
EXPLAIN (verbose, costs off)
UPDATE a29_joinme SET other = v.x FROM (SELECT a29_foo.f3 + 1 AS x, a29_foo.f2
  FROM a29_foo JOIN a29_joinme ON f2 = f2j) v WHERE a29_joinme.f2j = v.f2;
UPDATE a29_joinme SET other = v.x FROM (SELECT a29_foo.f3 + 1 AS x, a29_foo.f2
  FROM a29_foo JOIN a29_joinme ON f2 = f2j) v WHERE a29_joinme.f2j = v.f2
  RETURNING a29_joinme.*, v.x;

-- A DELETE rule on the view, and one whose action reads a plain table too.
CREATE RULE a29_joinview_d AS ON DELETE TO a29_joinview DO INSTEAD
  DELETE FROM a29_foo USING a29_plain WHERE a29_foo.f2 = a29_plain.b AND a29_foo.f1 = old.f1
    RETURNING a29_foo.*, a29_plain.a;
EXPLAIN (costs off)
DELETE FROM a29_joinview WHERE f3 = 12 RETURNING *;
DELETE FROM a29_joinview WHERE f3 = 12 RETURNING *;
SELECT * FROM a29_foo ORDER BY f1;

-- A rule action with a FROM subquery reading OLD (LATERAL): *RULE* stays first.
CREATE TABLE a29_log (f1 int, n int);
CREATE RULE a29_joinview_u2 AS ON UPDATE TO a29_joinview DO ALSO
  INSERT INTO a29_log SELECT s.f1, s.n FROM (SELECT old.f1, count(*) AS n FROM a29_joinme WHERE f2j = old.f2) s;
UPDATE a29_joinview SET f3 = f3 + 1 WHERE f1 = 30;
SELECT * FROM a29_log;
SELECT * FROM a29_foo ORDER BY f1;

DROP VIEW a29_joinview;
DROP TABLE a29_foo, a29_joinme, a29_plain, a29_parent, a29_lone, a29_log;
