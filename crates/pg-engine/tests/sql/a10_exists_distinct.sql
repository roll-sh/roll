-- Round 15 (agent 10): EXISTS over a FROM subquery planned as a level of its own (DISTINCT):
-- the correlated clauses pushed down into it come back out over its outputs as the semi
-- join's (convert_EXISTS_sublink_to_join), under a nondeterministic collation too.
CREATE COLLATION a10_ecs (provider = icu, locale = '');
CREATE COLLATION a10_eci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE TABLE a10_t3 (x text COLLATE a10_ecs);
INSERT INTO a10_t3 VALUES ('abc'), ('ABC'), ('def'), ('ghi');
CREATE UNIQUE INDEX ON a10_t3 (x);
EXPLAIN (COSTS OFF)
SELECT * FROM a10_t3 t1
  WHERE EXISTS (SELECT 1 FROM (SELECT DISTINCT x FROM a10_t3) t2
                WHERE t1.x = t2.x COLLATE a10_eci)
ORDER BY 1;
SELECT * FROM a10_t3 t1
  WHERE EXISTS (SELECT 1 FROM (SELECT DISTINCT x FROM a10_t3) t2
                WHERE t1.x = t2.x COLLATE a10_eci)
ORDER BY 1;
SELECT * FROM a10_t3 t1 WHERE EXISTS (SELECT 1 FROM (SELECT DISTINCT x FROM a10_t3) t2 WHERE t1.x = t2.x) ORDER BY 1;
SELECT * FROM a10_t3 t1
  WHERE NOT EXISTS (SELECT 1 FROM (SELECT DISTINCT x FROM a10_t3) t2
                WHERE t1.x = t2.x COLLATE a10_eci AND t2.x <> 'def')
ORDER BY 1;
SELECT * FROM a10_t3 t1
  WHERE EXISTS (SELECT 1 FROM (SELECT DISTINCT x FROM a10_t3 WHERE x > 'b') t2 WHERE t1.x < t2.x)
ORDER BY 1;
DROP TABLE a10_t3;
DROP COLLATION a10_ecs;
DROP COLLATION a10_eci;
