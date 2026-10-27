-- Grouping sets: an untyped literal an operator makes a constant equal to a grouping key
-- without columns is an occurrence of that key (parse analysis matches the transformed
-- expressions): NULL in the empty grouping set's row, not folded with its surroundings.
CREATE TABLE a75gk (d date, n int);
SELECT count(*) FROM a75gk GROUP BY GROUPING SETS ((), ('2025-06-19'::date)) HAVING 1 > (10.00 - ('2025-06-19' - '1999-11-11'::date));
SELECT count(*), ('2025-06-19' - '1999-11-11'::date) AS x FROM a75gk GROUP BY GROUPING SETS ((), ('2025-06-19'::date));
SELECT count(*), '2025-06-19' - '1999-11-11'::date AS x FROM a75gk GROUP BY ROLLUP (('2025-06-19'::date));
INSERT INTO a75gk VALUES ('2024-01-01', 1);
SELECT count(*), ('2025-06-19' - '1999-11-11'::date) AS x, n FROM a75gk GROUP BY GROUPING SETS ((), ('2025-06-19'::date), (n)) ORDER BY 3, 2;
SELECT count(*), (d - '2024-01-01') AS x FROM a75gk GROUP BY GROUPING SETS ((), ('2024-01-01'::date), (d)) ORDER BY 2;
DROP TABLE a75gk;
