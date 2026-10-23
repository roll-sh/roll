-- Row marks and names of a modifying statement's other relations and of a locking level: a
-- rule action over an inheritance parent (rows), a non-pulled-up FROM subquery's whole-row
-- mark under FOR UPDATE of an inheritance tree (the parent's tableoid after it), an UPDATE's
-- joined relations (rows), and a MERGE whose source subquery's sub-select output is copied
-- into each place reading it.
CREATE TABLE a9p (f1 int, f2 text, f3 int, f4 int8);
CREATE TABLE a9c (f5 int) INHERITS (a9p);
INSERT INTO a9p VALUES (1, 'one', 10, 100), (4, 'four', 40, 400);
INSERT INTO a9c VALUES (4, 'kid', 41, 401, 5), (7, 'seven', 70, 700, 8);
CREATE TABLE a9j (f2j text, other int);
INSERT INTO a9j VALUES ('one', 11), ('kid', 22), ('seven', 33);
CREATE RULE a9p_del AS ON DELETE TO a9p DO INSTEAD
  UPDATE a9p SET f2 = f2 || ' (deleted)', f3 = -1, f4 = -1 WHERE f1 = OLD.f1
  RETURNING *;
DELETE FROM a9p WHERE f1 = 4 RETURNING old.*, new.*, *;
SELECT tableoid::regclass, * FROM a9p ORDER BY f1, f2;
DROP RULE a9p_del ON a9p;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT p.f1, s.c FROM a9p p, (SELECT f2j, count(*) c FROM a9j GROUP BY f2j) s
  WHERE p.f2 = s.f2j FOR UPDATE OF p;
SELECT p.f1, s.c FROM a9p p, (SELECT f2j, count(*) c FROM a9j GROUP BY f2j) s
  WHERE p.f2 = s.f2j ORDER BY 1 FOR UPDATE OF p;
SELECT p.f1, s.f2j FROM a9p p, (SELECT * FROM a9j OFFSET 0) s WHERE p.f2 = s.f2j ORDER BY 1 FOR SHARE;
SELECT * FROM (SELECT count(*) FROM a9j) s FOR UPDATE;
SELECT * FROM (SELECT f2j FROM a9j UNION SELECT 'x') s FOR SHARE;
SELECT * FROM (SELECT * FROM (SELECT count(*) c FROM a9j) a) b FOR UPDATE OF b;
SELECT * FROM (SELECT * FROM (SELECT count(*) c FROM a9j) a) b, a9j j WHERE j.other = 11 FOR UPDATE OF j;
UPDATE a9p SET f3 = f3 + j.other FROM a9j j, (SELECT f2j FROM a9j OFFSET 0) s
  WHERE a9p.f2 = j.f2j AND j.f2j = s.f2j RETURNING a9p.f1, j.other;
CREATE TABLE a9src (a int, b int, c int, d int);
CREATE TABLE a9tgt (a int, b int, c int, d int);
CREATE TABLE a9ref (ab int, cd int);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a9tgt t
USING (SELECT *, (SELECT count(*) FROM a9ref r
                   WHERE r.ab = s.a + s.b AND r.cd = s.c - s.d) cnt
         FROM a9src s) s
ON t.a = s.a AND t.b < s.cnt
WHEN MATCHED AND t.c > s.cnt THEN
  UPDATE SET (b, c) = (SELECT s.b, s.cnt);
INSERT INTO a9src VALUES (1, 2, 3, 4), (2, 3, 4, 5), (3, 4, 5, 6);
INSERT INTO a9tgt VALUES (1, 0, 9, 0), (2, 0, 9, 0), (3, 5, 0, 0);
INSERT INTO a9ref VALUES (3, -1), (3, -1), (5, -1);
MERGE INTO a9tgt t
USING (SELECT *, (SELECT count(*) FROM a9ref r
                   WHERE r.ab = s.a + s.b AND r.cd = s.c - s.d) cnt
         FROM a9src s) s
ON t.a = s.a AND t.b < s.cnt
WHEN MATCHED AND t.c > s.cnt THEN
  UPDATE SET (b, c) = (SELECT s.b, s.cnt)
WHEN NOT MATCHED THEN INSERT VALUES (s.a + 10, s.cnt, s.cnt, 0);
SELECT * FROM a9tgt ORDER BY a, b;
MERGE INTO a9tgt t
USING (SELECT *, (SELECT count(*) FROM a9ref r WHERE r.ab = s.a + s.b) cnt FROM a9src s) s
ON t.a = s.a
WHEN NOT MATCHED BY SOURCE THEN DELETE
WHEN MATCHED AND s.cnt > 1 THEN UPDATE SET d = s.cnt
RETURNING merge_action(), t.*, s.cnt;
SELECT * FROM a9tgt ORDER BY a, b;
DROP TABLE a9src, a9tgt, a9ref, a9j, a9c, a9p;
