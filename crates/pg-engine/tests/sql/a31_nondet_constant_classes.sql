-- A constant equated with a column under a nondeterministic collation joins the class of a
-- join clause of the same collation (process_equivalence merges clauses of the class's
-- ec_collation only): the other side gets `x = k` and the join clause goes
-- (generate_base_implied_equalities_const), each member's clause rebuilt over the class's
-- collation (canonicalize_ec_expression: a column of another collation relabeled,
-- `(x)::text`, the constant `'k'::text COLLATE c`). A constant of another collation's class
-- (a deterministic join clause with a nondeterministic constant, or the other way round)
-- stays on its side: equal under the constant's collation isn't equal under the join's, so
-- deriving across them returned rows PostgreSQL doesn't.
CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE n1 (x text COLLATE ci, y int);
CREATE TABLE n2 (x text COLLATE ci, z text, y int);
CREATE TABLE n3 (x text, y int);
CREATE TABLE n4 (x text, y int);
INSERT INTO n1 SELECT chr(97 + i % 5) || (i % 3), i FROM generate_series(1, 200) i;
INSERT INTO n2 SELECT upper(chr(97 + i % 5)) || (i % 3), chr(97 + i % 5) || (i % 3), i FROM generate_series(1, 300) i;
INSERT INTO n3 SELECT chr(97 + i % 5) || (i % 3), i FROM generate_series(1, 30) i;
INSERT INTO n4 SELECT upper(chr(97 + i % 5)) || (i % 3), i FROM generate_series(1, 30) i;
INSERT INTO n4 SELECT chr(97 + i % 5) || (i % 3), i FROM generate_series(1, 6) i;
CREATE INDEX ON n2 (x);
ANALYZE n1, n2, n3, n4;

-- Columns of the collation: the constant crosses, the join clause goes.
EXPLAIN (COSTS OFF) SELECT * FROM n1, n2 WHERE n1.x = n2.x AND n1.x = 'a1';
SELECT count(*) FROM n1, n2 WHERE n1.x = n2.x AND n1.x = 'a1';
EXPLAIN (COSTS OFF) SELECT * FROM n1, n2 WHERE n1.x = n2.x AND n2.x = 'A1';
SELECT count(*) FROM n1, n2 WHERE n1.x = n2.x AND n2.x = 'A1';
EXPLAIN (COSTS OFF) SELECT * FROM n1 JOIN n2 ON n1.x = n2.x WHERE n1.x = 'b2' AND n1.y < 50;
SELECT count(*) FROM n1 JOIN n2 ON n1.x = n2.x WHERE n1.x = 'b2' AND n1.y < 50;
EXPLAIN (COSTS OFF) SELECT * FROM n1 JOIN n2 ON n1.x = n2.x JOIN n3 ON n3.y = n2.y WHERE n2.x = 'c0';
SELECT count(*) FROM n1 JOIN n2 ON n1.x = n2.x JOIN n3 ON n3.y = n2.y WHERE n2.x = 'c0';
-- Columns of another collation compared under it: relabeled.
EXPLAIN (COSTS OFF) SELECT * FROM n3 a, n4 b WHERE a.x = b.x COLLATE ci AND b.x = 'a1' COLLATE ci;
SELECT a.x, b.x, count(*) FROM n3 a, n4 b WHERE a.x = b.x COLLATE ci AND b.x = 'a1' COLLATE ci GROUP BY 1, 2 ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT * FROM n1, n2, n3 WHERE n1.x = n2.x AND n2.x = n3.x COLLATE ci AND n3.x = 'b2' COLLATE ci;
SELECT count(*) FROM n1, n2, n3 WHERE n1.x = n2.x AND n2.x = n3.x COLLATE ci AND n3.x = 'b2' COLLATE ci;
EXPLAIN (COSTS OFF) SELECT * FROM n1, n2 WHERE n1.x = n2.z COLLATE ci AND n2.z = 'a1' COLLATE ci;
SELECT count(*) FROM n1, n2 WHERE n1.x = n2.z COLLATE ci AND n2.z = 'a1' COLLATE ci;
-- Classes of different collations: nothing crosses.
EXPLAIN (COSTS OFF) SELECT * FROM n3 a, n4 b WHERE a.x = b.x AND a.x = 'A1' COLLATE ci;
SELECT a.x, b.x, count(*) FROM n3 a, n4 b WHERE a.x = b.x AND a.x = 'A1' COLLATE ci GROUP BY 1, 2 ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT * FROM n3 a, n4 b WHERE a.x = b.x COLLATE ci AND b.x = 'a1';
SELECT a.x, b.x, count(*) FROM n3 a, n4 b WHERE a.x = b.x COLLATE ci AND b.x = 'a1' GROUP BY 1, 2 ORDER BY 1, 2;
-- An outer join: the nullable side gets the constant, the other keeps its clause as written.
EXPLAIN (COSTS OFF) SELECT * FROM n1 LEFT JOIN n2 ON n1.x = n2.x WHERE n1.x = 'a1';
SELECT count(*) FROM n1 LEFT JOIN n2 ON n1.x = n2.x WHERE n1.x = 'a1';
-- A semi join: the sub-select's own restriction rebuilt too.
EXPLAIN (COSTS OFF) SELECT * FROM n1 WHERE n1.x IN (SELECT x FROM n2 WHERE n2.x = 'D2');
SELECT count(*) FROM n1 WHERE n1.x IN (SELECT x FROM n2 WHERE n2.x = 'D2');
