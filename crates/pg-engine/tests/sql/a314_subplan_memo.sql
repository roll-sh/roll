-- Correlated scalar and EXISTS SubPlans of the statement's own level run once for each set of
-- their parameters' values (their rows remembered for the statement): the same rows, errors
-- and notices as a run for every outer row.
CREATE TABLE a314_p (k int PRIMARY KEY, brand text);
CREATE TABLE a314_l (k int, q int, price numeric);
INSERT INTO a314_p SELECT g, 'b' || (g % 3) FROM generate_series(1, 30) g;
INSERT INTO a314_l SELECT g % 30 + 1, g % 17, g * 1.5 FROM generate_series(1, 600) g;
INSERT INTO a314_l VALUES (NULL, 4, 1), (NULL, 9, 2);
CREATE INDEX a314_l_k ON a314_l (k);
ANALYZE a314_p, a314_l;
-- TPC-H q17's shape: the average of each part's rows, for each of its rows
SELECT round(sum(l.price) / 7.0, 4) AS avg_yearly, count(*) FROM a314_l l JOIN a314_p p ON p.k = l.k
WHERE p.brand = 'b1' AND l.q < (SELECT 0.5 * avg(q) FROM a314_l WHERE k = p.k);
-- two parameters, each pair repeated, and NULL ones
SELECT l.k, count(*), sum((SELECT max(x.price) FROM a314_l x WHERE x.k = l.k AND x.q < l.q))
FROM a314_l l WHERE l.k <= 4 OR l.k IS NULL GROUP BY l.k ORDER BY l.k;
-- EXISTS under an OR stays a SubPlan
SELECT count(*) FROM a314_l l WHERE l.q > 15 OR EXISTS (SELECT 1 FROM a314_l x WHERE x.k = l.k AND x.q = 16 AND x.price > 800);
-- more than one row for one parameter: the error when that value comes
CREATE TABLE a314_d (k int, v int);
INSERT INTO a314_d VALUES (1, 1), (2, 2), (2, 3);
SELECT l.k, (SELECT v FROM a314_d d WHERE d.k = l.k) FROM (VALUES (1), (1), (3), (1)) l(k);
SELECT l.k, (SELECT v FROM a314_d d WHERE d.k = l.k) FROM (VALUES (1), (1), (2)) l(k);
-- a user function inside raises its NOTICE at every run
CREATE FUNCTION a314_f(int) RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'run %', $1; RETURN $1 * 2; END $$;
SELECT l.k, (SELECT a314_f(d.v) FROM a314_d d WHERE d.k = l.k AND d.v < 3) FROM (VALUES (1), (1), (2), (2)) l(k);
-- an UPDATE's SubPlan reads the rows as of the statement's start, whatever it changed
UPDATE a314_l u SET q = (SELECT max(q) FROM a314_l x WHERE x.k = u.k) + 100 WHERE u.k <= 2;
SELECT k, min(q), max(q), count(*) FROM a314_l WHERE k <= 3 GROUP BY k ORDER BY k;
