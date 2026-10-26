-- A LATERAL subquery's WHERE (or inner join) clause reading only the left side's columns,
-- pulled up: it is the LATERAL join's own clause (PostgreSQL postpones it to the nearest
-- join level with every relation it reads), never a restriction of the left side.
CREATE TABLE a68l_a (id int PRIMARY KEY, x int, f bool);
CREATE TABLE a68l_b (id int PRIMARY KEY, y int);
INSERT INTO a68l_a VALUES (1, 10, true), (2, NULL, false), (3, 30, NULL), (4, 5, true);
INSERT INTO a68l_b VALUES (1, 10), (2, 20), (5, NULL);
SELECT a.id, s.c FROM a68l_a a LEFT JOIN LATERAL (SELECT b.id AS c FROM a68l_b b, a68l_b d WHERE 2 >= a.id AND b.id = d.id) s ON true ORDER BY 1, 2;
SELECT a.id, s.c FROM a68l_a a LEFT JOIN LATERAL (SELECT 1 AS c FROM a68l_b b, a68l_b d WHERE a.x > 7) s ON a.f ORDER BY 1, 2;
SELECT a.id, s.c FROM a68l_a a LEFT JOIN LATERAL (SELECT b.y AS c FROM a68l_b b JOIN a68l_b d ON a.x IS NOT NULL AND b.id = d.id) s ON true ORDER BY 1, 2;
SELECT a.id, s.c FROM a68l_a a LEFT JOIN LATERAL (SELECT a.x AS c FROM a68l_b b, a68l_b d WHERE a.x < 20 AND d.y = b.y) s ON true ORDER BY 1, 2;
SELECT count(*), count(s.c) FROM a68l_a a LEFT JOIN LATERAL (SELECT b.id AS c FROM a68l_b b, a68l_b d, a68l_b e WHERE a.id <> 3) s ON true;
-- under an outer join below (a column of its nullable side)
SELECT a.id, c.id, s.c FROM (a68l_a a LEFT JOIN a68l_b c ON a.id = c.id + 1) LEFT JOIN LATERAL (SELECT b.id AS c FROM a68l_b b, a68l_b d WHERE 1 <= c.id AND b.id = d.id) s ON true ORDER BY 1, 2, 3;
SELECT count(*) FROM (a68l_a a LEFT JOIN unnest('{1,5}'::int4[]) WITH ORDINALITY u(v, o) ON a.id = u.v) LEFT JOIN LATERAL (SELECT 1 FROM (a68l_b b JOIN (VALUES (0), (1)) w(z) ON 1 <= a.id) WHERE 1 BETWEEN 1 AND u.o) s ON false;
SELECT a.id, c.id, s.c FROM (a68l_a a LEFT JOIN a68l_b c ON a.id = c.id) LEFT JOIN LATERAL (SELECT b.id AS c FROM a68l_b b JOIN a68l_b d ON b.id = d.id WHERE c.y IS NOT NULL AND a.f) s ON a.id < 4 ORDER BY 1, 2, 3;
-- inner LATERAL joins: the clause filters the join's rows
SELECT a.id, s.c FROM a68l_a a, LATERAL (SELECT b.id AS c FROM a68l_b b, a68l_b d WHERE a.f AND b.id = d.id) s ORDER BY 1, 2;
SELECT a.id, c.id, s.c FROM a68l_a a LEFT JOIN a68l_b c ON a.id = c.id CROSS JOIN LATERAL (SELECT b.id AS c FROM a68l_b b, a68l_b d WHERE c.y > 5 AND b.id = d.id) s ORDER BY 1, 2, 3;
SELECT a.id, s.c FROM a68l_a a JOIN LATERAL (SELECT b.id AS c FROM a68l_b b, a68l_b d WHERE a.x > 6 AND b.id = d.id) s ON s.c < 3 ORDER BY 1, 2;
-- nested: a LATERAL subquery inside another's nullable side
SELECT a.id, s.c, s.e FROM a68l_a a LEFT JOIN LATERAL (SELECT b.id AS c, t.e FROM a68l_b b LEFT JOIN LATERAL (SELECT d.id AS e FROM a68l_b d, a68l_b g WHERE b.y > 15 AND a.x > 7) t ON true) s ON true ORDER BY 1, 2, 3;
DROP TABLE a68l_a, a68l_b;
