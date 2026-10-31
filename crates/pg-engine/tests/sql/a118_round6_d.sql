-- Difftest round 6, batch 2 (agent 118): a call written in SQL syntax (EXTRACT, SUBSTRING, POSITION) is as strict as its function: outer joins under it reduce.
CREATE TABLE x1 (a int, b date);
CREATE TABLE x3 (a int, b date, c int, s text);
INSERT INTO x1 VALUES (1, '2025-01-05');
INSERT INTO x3 VALUES (1, '2025-01-05', 0, 'abc'), (2, '2025-01-06', 1, NULL);
SELECT * FROM x1 FULL JOIN x3 t5 ON (SELECT true) WHERE extract(dow FROM t5.b) = 0;
SELECT * FROM x1 FULL JOIN x3 t5 ON x1.a > t5.a WHERE extract(dow FROM t5.b) = 0;
EXPLAIN (COSTS OFF) SELECT * FROM x1 LEFT JOIN x3 t5 ON x1.a = t5.a WHERE extract(dow FROM t5.b) > 0;
EXPLAIN (COSTS OFF) SELECT * FROM x1 LEFT JOIN x3 t5 ON x1.a = t5.a WHERE substring(t5.s FROM 1 FOR 1) = 'a';
EXPLAIN (COSTS OFF) SELECT * FROM x1 LEFT JOIN x3 t5 ON x1.a = t5.a WHERE position('b' IN t5.s) IS NOT NULL;
SELECT x1.a, t5.s FROM x1 LEFT JOIN x3 t5 ON x1.a = t5.a WHERE position('b' IN t5.s) IS NOT NULL;
