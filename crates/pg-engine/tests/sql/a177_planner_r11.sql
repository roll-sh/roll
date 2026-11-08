-- Planner round 11 (agent 177).
-- DISTINCT and GROUP BY over a written COLLATE: the CollateExpr is a RelabelType (none
-- when the collation is the column's own), shown as a cast with the type's modifier in the
-- grouping's keys and under the collation in a sort above.
CREATE TABLE a177_d (t text, u varchar(10), c text COLLATE "C", n int);
INSERT INTO a177_d SELECT 'x' || (g % 50), 'y' || (g % 7), 'z' || (g % 9), g FROM generate_series(1, 1000) g;
ANALYZE a177_d;
EXPLAIN (COSTS OFF) SELECT DISTINCT t COLLATE "C" FROM a177_d;
EXPLAIN (COSTS OFF) SELECT DISTINCT t COLLATE "en-x-icu" FROM a177_d;
EXPLAIN (COSTS OFF) SELECT DISTINCT u COLLATE "C" FROM a177_d;
EXPLAIN (COSTS OFF) SELECT DISTINCT c COLLATE "C" FROM a177_d;
EXPLAIN (COSTS OFF) SELECT DISTINCT t COLLATE "default" FROM a177_d;
EXPLAIN (COSTS OFF) SELECT DISTINCT t COLLATE "C" FROM a177_d ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT DISTINCT u COLLATE "C" FROM a177_d ORDER BY 1 DESC;
EXPLAIN (COSTS OFF) SELECT u COLLATE "C", count(*) FROM a177_d GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT t FROM a177_d ORDER BY u COLLATE "C";
SELECT DISTINCT t COLLATE "C" FROM a177_d ORDER BY 1 LIMIT 3;
SELECT DISTINCT u COLLATE "C" FROM a177_d ORDER BY 1 DESC LIMIT 3;
DROP TABLE a177_d;
