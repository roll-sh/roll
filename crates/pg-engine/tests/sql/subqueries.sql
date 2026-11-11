CREATE TABLE emp (id int PRIMARY KEY, name text, dept text, boss int, salary int);
INSERT INTO emp VALUES (1, 'ada', 'eng', NULL, 300), (2, 'bo', 'eng', 1, 200), (3, 'cy', 'ops', 1, 150), (4, 'di', 'ops', 3, 120), (5, 'ed', 'eng', 2, 180), (6, 'fy', NULL, NULL, 90);
-- scalar, IN, EXISTS, ANY/ALL
SELECT name, (SELECT name FROM emp b WHERE b.id = e.boss) AS boss FROM emp e ORDER BY id;
SELECT name FROM emp WHERE salary > (SELECT avg(salary) FROM emp) ORDER BY name;
SELECT name FROM emp e WHERE EXISTS (SELECT 1 FROM emp r WHERE r.boss = e.id) ORDER BY name;
SELECT name FROM emp e WHERE NOT EXISTS (SELECT 1 FROM emp r WHERE r.boss = e.id) ORDER BY name;
SELECT name FROM emp WHERE id IN (SELECT boss FROM emp) ORDER BY name;
SELECT name FROM emp WHERE id NOT IN (SELECT boss FROM emp WHERE boss IS NOT NULL) ORDER BY name;
SELECT name FROM emp WHERE id NOT IN (SELECT boss FROM emp) ORDER BY name;
SELECT name FROM emp WHERE salary >= ALL (SELECT salary FROM emp WHERE dept = 'eng') ORDER BY name;
SELECT name FROM emp WHERE salary < ANY (SELECT salary FROM emp WHERE dept = 'ops') ORDER BY name;
SELECT (SELECT id FROM emp);
SELECT (SELECT id, name FROM emp LIMIT 1);
SELECT name FROM emp WHERE (dept, salary) IN (SELECT dept, max(salary) FROM emp GROUP BY dept) ORDER BY name;
-- derived tables and lateral
SELECT d.dept, d.total FROM (SELECT dept, sum(salary) AS total FROM emp GROUP BY dept) d WHERE d.total > 200 ORDER BY 1;
SELECT e.name, top.name AS top_report FROM emp e, LATERAL (SELECT r.name FROM emp r WHERE r.boss = e.id ORDER BY r.salary DESC LIMIT 1) top ORDER BY 1;
SELECT e.name, x.n FROM emp e LEFT JOIN LATERAL (SELECT count(*) AS n FROM emp r WHERE r.boss = e.id) x ON true ORDER BY 1;
SELECT * FROM (VALUES (1, 'one'), (2, 'two')) AS v(n, word) ORDER BY n DESC;
-- CTEs
WITH eng AS (SELECT * FROM emp WHERE dept = 'eng') SELECT name FROM eng ORDER BY salary;
WITH a AS (SELECT 1 AS x), b AS (SELECT x + 1 AS y FROM a) SELECT * FROM a, b;
WITH RECURSIVE chain AS (
  SELECT id, name, boss, 1 AS depth FROM emp WHERE id = 5
  UNION ALL
  SELECT e.id, e.name, e.boss, c.depth + 1 FROM emp e JOIN chain c ON e.id = c.boss
) SELECT name, depth FROM chain ORDER BY depth;
WITH RECURSIVE tree AS (
  SELECT id, name, ARRAY[name] AS path FROM emp WHERE boss IS NULL AND dept IS NOT NULL
  UNION ALL
  SELECT e.id, e.name, t.path || e.name FROM emp e JOIN tree t ON e.boss = t.id
) SELECT array_to_string(path, ' > ') FROM tree ORDER BY path;
WITH RECURSIVE n(i) AS (SELECT 1 UNION SELECT i + 1 FROM n WHERE i < 5) SELECT sum(i) FROM n;
WITH RECURSIVE fib(a, b) AS (VALUES (0::numeric, 1::numeric) UNION ALL SELECT b, a + b FROM fib WHERE b < 100) SELECT string_agg(a::text, ',') FROM fib;
WITH m AS MATERIALIZED (SELECT id FROM emp WHERE salary > 150) SELECT count(*) FROM m;
WITH upd AS (UPDATE emp SET salary = salary + 10 WHERE dept = 'ops' RETURNING id, salary) SELECT * FROM upd ORDER BY id;
WITH del AS (DELETE FROM emp WHERE dept IS NULL RETURNING name) INSERT INTO emp (id, name, dept, salary) SELECT 7, name || '2', 'hr', 1 FROM del RETURNING id, name;
WITH RECURSIVE bad AS (SELECT 1 UNION ALL SELECT * FROM bad) SELECT * FROM bad LIMIT 3;
WITH RECURSIVE g AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM g WHERE n < 3) SEARCH DEPTH FIRST BY n SET ord SELECT * FROM g;
-- set operations
SELECT dept FROM emp UNION SELECT 'qa' ORDER BY 1;
SELECT dept FROM emp UNION ALL SELECT dept FROM emp WHERE id < 3 ORDER BY 1 NULLS FIRST;
SELECT dept FROM emp INTERSECT SELECT 'eng' ORDER BY 1;
SELECT dept FROM emp EXCEPT SELECT 'eng' ORDER BY 1;
SELECT 1 UNION SELECT 'a';
(SELECT name FROM emp ORDER BY salary DESC LIMIT 2) UNION ALL (SELECT 'x') ORDER BY 1;
DROP TABLE emp;
