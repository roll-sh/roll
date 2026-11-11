-- Grouping sets: a GROUP BY item without columns (written as an expression) that occurs
-- inside a select-list item or HAVING is a value of the group's rows there, its
-- surroundings evaluated above it: NULL in the empty grouping set's row over no input,
-- even where folding would have merged it with its surroundings (`abs(9) * 2`).
CREATE TABLE a68g (x int);
CREATE TABLE a68ge (x int);
INSERT INTO a68g VALUES (1), (2);
SELECT abs(9) + 1 AS a, count(*) FROM a68g GROUP BY ROLLUP (abs(9));
SELECT (abs(9))::int2 AS a, count(*) FROM a68g GROUP BY ROLLUP (abs(9));
SELECT (position('bar' in 'foo'))::int2 AS a, count(*) FROM a68g GROUP BY CUBE (x, position('bar' in 'foo')) ORDER BY 2, 1;
SELECT (position('bar' in 'foo'))::int2 AS a, count(*) FROM a68ge GROUP BY CUBE (x, position('bar' in 'foo'));
SELECT 1 AS z, abs(9) AS a, abs(9) * 2 AS b, count(*) FROM a68g GROUP BY ROLLUP (x, abs(9)) ORDER BY 4, 2;
SELECT abs(9) * 2 AS b, count(*) FROM a68ge GROUP BY ROLLUP (x, abs(9));
SELECT (1 + 1) * 3 AS a, count(*) FROM a68ge GROUP BY ROLLUP (x, 1 + 1);
SELECT lower('A') || 'b' AS a, count(*) FROM a68g GROUP BY GROUPING SETS ((x), (lower('A'))) ORDER BY 2;
SELECT 'h1', count(*) FROM a68ge GROUP BY ROLLUP (x, abs(9)) HAVING abs(9) + 1 = 10;
SELECT 'h2', count(*) FROM a68g GROUP BY ROLLUP (x, abs(9)) HAVING abs(9) + 1 = 10 ORDER BY 2;
SELECT 'h3', count(*) FROM a68ge GROUP BY ROLLUP (x, abs(9)) HAVING abs(9) + 1 IS NULL;
SELECT 'h4', count(*) FROM a68ge GROUP BY ROLLUP (x, abs(9)) HAVING abs(9) = 9;
SELECT 'h6', abs(9) * 2 + 1 AS b, count(*) FROM a68ge GROUP BY ROLLUP (x, abs(9));
SELECT 'h7', abs(9) * 2 + count(*) AS b FROM a68ge GROUP BY ROLLUP (x, abs(9));
SELECT 'h8', abs(9) * 2 AS b, 5 AS c FROM a68ge GROUP BY ROLLUP (x, abs(9));
SELECT 'h9', abs(9) * 2 AS b, count(*) FROM a68ge GROUP BY GROUPING SETS ((x, abs(9)), ());
SELECT 'h10', abs(9) * 2 AS b, count(*) FROM a68ge GROUP BY GROUPING SETS ((abs(9)), ());
SELECT 'h11', sum(abs(9) * 2) AS s, abs(9) * x AS m FROM a68g GROUP BY ROLLUP (x, abs(9)) ORDER BY 3, 2;
-- a typed literal key
SELECT 'l1', ('2025-06-19'::date) - '1999-11-11'::date AS v, count(*) FROM a68ge GROUP BY ROLLUP (x, ('2025-06-19'::date));
SELECT 'l4', 'ab'::text || 'c' AS v, count(*) FROM a68ge GROUP BY ROLLUP (x, 'ab'::text);
DROP TABLE a68g, a68ge;
