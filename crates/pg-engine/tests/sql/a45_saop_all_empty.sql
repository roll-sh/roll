-- Found by the differential tester (tests/difftest.rs). `x op ALL (array)` is true over an
-- empty array whatever x is (is_strict_saop): a WHERE clause of it doesn't make a LEFT JOIN
-- an inner one unless the array is known to have elements.
CREATE TABLE a (x int, y int[]);
CREATE TABLE b (x int, z int);
INSERT INTO a VALUES (1, '{}'), (2, '{1}');
INSERT INTO b VALUES (2, 5);
SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE b.z < ALL ('{}') ORDER BY 1;
SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE b.z = ALL (ARRAY[]::int[]) ORDER BY 1;
SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE b.z < ALL (a.y) ORDER BY 1;
SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE b.z < ALL ('{7}') ORDER BY 1;
SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE b.z = ANY ('{}') ORDER BY 1;
SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE NOT (b.z = ANY ('{}')) ORDER BY 1;
SELECT * FROM a LEFT JOIN (SELECT x, z, 1 AS k FROM b) s ON a.x = s.x WHERE s.z <> ALL ('{}') ORDER BY 1;
SELECT * FROM a LEFT JOIN b ON a.x = b.x LEFT JOIN b b2 ON b2.z < ALL ('{}') AND b.z IS NULL ORDER BY 1;
SELECT * FROM a FULL JOIN b ON a.x = b.x WHERE b.z < ALL ('{}') ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT * FROM a LEFT JOIN b ON a.x = b.x WHERE b.z < ALL ('{}');
