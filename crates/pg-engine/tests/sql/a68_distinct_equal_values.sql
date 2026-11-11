-- DISTINCT under an ORDER BY planned as a Unique over a Sort: of values that compare equal
-- but differ ('1 mon' / '30 days'), the one kept is the first of its group in the sort's
-- order (Postgres's quicksort), not the first one read (difftest seed 1777 #34).
CREATE TABLE a68d (a interval, b int4 DEFAULT 0, c numeric NULL, d int4);
INSERT INTO a68d (a, b, c, d) VALUES ('24:00:00', 8, 4.00, 3), ('1 mon', NULL, 0.05, - 2), ('30 days', 5, - 3.50, 4), ('2 hours', 5, - 3.25, 1), ('2 hours', 6, - 3.00, 7), ('30 days', NULL, 9.50, 9), (NULL, 8, 11.50, 5);
CREATE INDEX a68d_c ON a68d USING hash (c);
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM a68d ORDER BY 1;
SELECT DISTINCT a FROM a68d ORDER BY 1;
SELECT DISTINCT a, 1 AS c1 FROM a68d ORDER BY 1;
SELECT DISTINCT 1 AS c1, a FROM a68d ORDER BY 1, 2;
SELECT DISTINCT 'x' AS c0, a FROM a68d ORDER BY 1, 2;
SELECT DISTINCT '{1,2}'::int4[] AS c0, 1 AS c1, a FROM a68d ORDER BY 1, 3;
SELECT a, b, d FROM a68d ORDER BY a, b, d;
DROP TABLE a68d;
