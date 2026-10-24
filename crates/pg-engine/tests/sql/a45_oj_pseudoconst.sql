-- Found by the differential tester (tests/difftest.rs). A clause of an inner join that reads
-- no column of the query level (an uncorrelated sub-select, an InitPlan's output, a volatile
-- call) holds where it was written (distribute_qual_to_rels' qualscope); only a pseudoconstant
-- one in the top join domain gates the whole plan. Inside an outer join's nullable side it
-- gated the outer join, losing the preserved side's rows.
CREATE TABLE a (x int);
CREATE TABLE c (z int);
INSERT INTO a VALUES (1), (2);
INSERT INTO c VALUES (1), (3);
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false)) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON random() > 2) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON NOT EXISTS (SELECT 1 FROM a)) ON a1.x = c.z ORDER BY 1;
SELECT * FROM (a a1 JOIN a a2 ON (SELECT false)) RIGHT JOIN c ON a1.x = c.z ORDER BY 3;
SELECT * FROM c FULL JOIN (a a1 JOIN a a2 ON (SELECT false)) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN (a a2 JOIN a a3 ON (SELECT false)) ON a1.x = a2.x) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false)) ON true ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false)) ON a1.x = c.z LEFT JOIN a a3 ON a3.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false) JOIN a a3 ON a3.x = a1.x) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 LEFT JOIN (a a2 JOIN a a3 ON (SELECT false)) ON a2.x = a1.x) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON now() IS NULL) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT 1) = 2 AND a1.x = a2.x) ON a1.x = c.z ORDER BY 1;
SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON a1.x = a2.x AND (SELECT true)) ON a1.x = c.z ORDER BY 1;
SELECT * FROM (a a1 JOIN a a2 ON (SELECT false)) LEFT JOIN c ON a1.x = c.z;
SELECT * FROM c WHERE NOT EXISTS (SELECT 1 FROM a a1 JOIN a a2 ON (SELECT false) WHERE a1.x = c.z) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false)) ON a1.x = c.z;
EXPLAIN (COSTS OFF) SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON random() > 2) ON a1.x = c.z;
EXPLAIN (COSTS OFF) SELECT * FROM c FULL JOIN (a a1 JOIN a a2 ON (SELECT false)) ON a1.x = c.z;
EXPLAIN (COSTS OFF) SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false) JOIN a a3 ON a3.x = a1.x) ON a1.x = c.z;
EXPLAIN (COSTS OFF) SELECT * FROM (a a1 JOIN a a2 ON (SELECT false)) LEFT JOIN c ON a1.x = c.z;
EXPLAIN (COSTS OFF) SELECT * FROM (a a1 JOIN a a2 ON random() > 2) LEFT JOIN c ON a1.x = c.z;
EXPLAIN (COSTS OFF) SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON now() IS NULL) ON a1.x = c.z;
