-- Initplans read by a big table's scan filter, substring() of constants in filters and keys,
-- and groupings over joins through their column reorderings (agent 189, executor round 11).
CREATE TABLE a189_t (id int, v int, s text, d date);
INSERT INTO a189_t SELECT i, i % 100, md5(i::text), date '2000-01-01' + i % 3000 FROM generate_series(1, 200000) i;
CREATE TABLE a189_e (id int, v int, s text);
CREATE TABLE a189_dim (k int PRIMARY KEY, name text, w int);
INSERT INTO a189_dim SELECT k, 'name ' || (k % 7), k % 5 FROM generate_series(0, 99) k;
ANALYZE a189_t;
ANALYZE a189_dim;
-- An initplan read by the first condition, a substring() of constants in the second.
SELECT count(*), sum(id) FROM a189_t WHERE v > (SELECT avg(v) FROM a189_t WHERE id < 1000) AND substring(s FROM 1 FOR 1) IN ('a', 'b');
SELECT substring(s FROM 1 FOR 2) AS code, count(*), sum(v) FROM a189_t
  WHERE v > (SELECT avg(v) FROM a189_t WHERE id < 50) AND substring(s FROM 1 FOR 2) IN ('13', 'a1', 'ff', '0b')
  GROUP BY 1 ORDER BY 1;
-- An initplan that fails, over a table with rows: its error.
SELECT count(*) FROM a189_t WHERE v > (SELECT 1 / (min(v) - min(v)) FROM a189_t) AND s LIKE 'a%';
-- Over an empty table it never runs.
SELECT count(*) FROM a189_e WHERE v > (SELECT 1 / (count(*) - count(*))::int FROM a189_e) AND s LIKE 'a%';
-- substring() past either end, from below 1, of NULLs.
SELECT substring(s FROM -1 FOR 3) AS a, substring(s FROM 31 FOR 10) AS b, count(*)
  FROM a189_t WHERE substring(s FROM 0 FOR 2) = 'a' GROUP BY 1, 2 ORDER BY 1, 2 LIMIT 5;
SELECT count(substring(CASE WHEN id % 3 = 0 THEN NULL ELSE s END FROM 2 FOR 0)), count(*) FROM a189_t WHERE substring(s FROM 2 FOR 1) = '7';
-- Groupings over two joins with computed keys and arguments.
SELECT d.name, extract(year FROM t.d) AS y, count(*), sum(t.v * (1 - d.w)) FROM a189_t t
  JOIN a189_dim d ON d.k = t.v JOIN a189_dim e ON e.k = t.id % 100 WHERE e.w < 3
  GROUP BY 1, 2 ORDER BY 1, 2 DESC LIMIT 12;
SELECT k, n FROM (SELECT t.id % 5000 AS k, count(d.k) AS n FROM a189_t t LEFT JOIN a189_dim d ON d.k = t.v + 50 GROUP BY 1) x
  ORDER BY n DESC, k LIMIT 8;
DROP TABLE a189_t, a189_e, a189_dim;
