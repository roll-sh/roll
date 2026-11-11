-- EXISTS over a join that the subquery's own planning made a nested loop over a
-- parameterized index path (the inner scan reading the outer row's value): pulled up, the
-- join is the one written, its clause back in its condition (it used to keep the planned
-- path's parameter, which then read the semi join's: wrong rows, or "outer reference
-- without an outer row").
CREATE TABLE a51_author (id int PRIMARY KEY, name text, num int UNIQUE);
CREATE TABLE a51_report (id int PRIMARY KEY, name text, creator_id int REFERENCES a51_author(num));
CREATE INDEX ON a51_report (creator_id);
CREATE TABLE a51_note (id int PRIMARY KEY, note text);
CREATE TABLE a51_item (id int PRIMARY KEY, name text, creator_id int REFERENCES a51_author, note_id int REFERENCES a51_note);
CREATE INDEX ON a51_item (creator_id);
CREATE INDEX ON a51_item (note_id);
INSERT INTO a51_note VALUES (1, 'n1'), (2, 'n2'), (3, 'n3');
INSERT INTO a51_author VALUES (1, 'a1', 1001), (2, 'a2', 2002), (3, 'a3', 3003), (4, 'a4', 4004);
INSERT INTO a51_report VALUES (1, 'r1', 1001), (2, 'r2', 3003), (3, 'r3', NULL);
INSERT INTO a51_item VALUES (1, 'one', 1, 3), (2, 'two', 2, 2), (3, 'three', 2, 3), (4, 'four', 4, 3);

EXPLAIN (COSTS OFF)
SELECT v0.id FROM a51_author v0 WHERE EXISTS
  (SELECT 1 FROM a51_report u0 JOIN a51_author u1 ON u0.creator_id = u1.num WHERE u1.id = v0.id);
SELECT v0.id FROM a51_author v0 WHERE EXISTS
  (SELECT 1 FROM a51_report u0 JOIN a51_author u1 ON u0.creator_id = u1.num WHERE u1.id = v0.id)
ORDER BY 1;
SELECT v0.id FROM a51_author v0 WHERE NOT EXISTS
  (SELECT 1 FROM a51_report u0 JOIN a51_author u1 ON u0.creator_id = u1.num WHERE u1.id = v0.id)
ORDER BY 1;
SELECT v0.id FROM a51_author v0 WHERE EXISTS
  (SELECT 1 FROM a51_report u0 LEFT JOIN a51_author u1 ON u0.creator_id = u1.num WHERE u0.creator_id = v0.num)
ORDER BY 1;

-- Django's Exists(...) of an OuterRef in an exclude (queries test_combining_does_not_mutate).
SELECT i.name FROM a51_item i JOIN a51_note n ON i.note_id = n.id
WHERE i.creator_id IN
    (SELECT w0.id FROM a51_author w0
     WHERE NOT (w0.id IN
                  (SELECT v0.id FROM a51_author v0
                   WHERE EXISTS
                       (SELECT 1 FROM a51_report u0 JOIN a51_author u1 ON u0.creator_id = u1.num
                        WHERE u1.id = v0.id LIMIT 1))))
ORDER BY n.note DESC, i.name;

-- The correlation inside an expression of the subquery's WHERE (expressions
-- test_annotation_with_deeply_nested_outerref, test_slicing_of_outerref).
CREATE TABLE a51_em (id int PRIMARY KEY, firstname text);
CREATE TABLE a51_co (id int PRIMARY KEY, name text, ceo_id int REFERENCES a51_em);
INSERT INTO a51_em VALUES (1, 'Joe'), (2, 'Frank'), (3, 'Max');
INSERT INTO a51_co VALUES (1, 'Example Inc.', 1), (2, 'Foobar Ltd.', 2), (3, 'Test GmbH', 3), (4, 'Jxx', 1);
EXPLAIN (COSTS OFF)
SELECT co.name FROM a51_co co JOIN a51_em em ON co.ceo_id = em.id
WHERE EXISTS (SELECT 1 FROM a51_co u0 JOIN a51_em u1 ON u0.ceo_id = u1.id
              WHERE u0.name LIKE substring(em.firstname, 1, 1) || '%' LIMIT 1);
SELECT co.name FROM a51_co co JOIN a51_em em ON co.ceo_id = em.id
WHERE EXISTS (SELECT 1 FROM a51_co u0 JOIN a51_em u1 ON u0.ceo_id = u1.id
              WHERE u0.name LIKE substring(em.firstname, 1, 1) || '%' LIMIT 1)
ORDER BY 1;
SELECT em.id FROM a51_em em WHERE EXISTS
  (SELECT 1 FROM a51_co u0 JOIN a51_em u1 ON u0.ceo_id = u1.id WHERE u0.name = em.firstname);
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (COSTS OFF)
SELECT co.name FROM a51_co co JOIN a51_em em ON co.ceo_id = em.id
WHERE EXISTS (SELECT 1 FROM a51_co u0 JOIN a51_em u1 ON u0.ceo_id = u1.id
              WHERE u0.name LIKE substring(em.firstname, 1, 1) || '%' LIMIT 1);
SELECT co.name FROM a51_co co JOIN a51_em em ON co.ceo_id = em.id
WHERE EXISTS (SELECT 1 FROM a51_co u0 JOIN a51_em u1 ON u0.ceo_id = u1.id
              WHERE u0.name LIKE substring(em.firstname, 1, 1) || '%' LIMIT 1)
ORDER BY 1;
RESET enable_hashjoin;
RESET enable_mergejoin;
