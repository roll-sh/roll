-- A FROM subquery found empty under a level found empty too: an output whose constant fails
-- to fold still fails the statement when the query above reads it (pull_up_subqueries
-- substitutes it before eval_const_expressions), and doesn't when nothing reads it.
CREATE TABLE a68f (id int4, a numeric(10,2));
CREATE TABLE a68f0 (id int4 PRIMARY KEY, b json);
CREATE VIEW a68fv AS SELECT '"s"' AS c0, t2.id AS c1, jsonb_array_length('{"b": [1, "a"]}') AS c3 FROM a68f t1 CROSS JOIN a68f0 t2 WHERE NULL;
SELECT 1, count(*) FROM (SELECT jsonb_array_length('{"b": 1}') AS c3 FROM a68f WHERE NULL) t WHERE NULL GROUP BY t.c3;
SELECT t.c3 FROM (SELECT jsonb_array_length('{"b": 1}') AS c3 FROM a68f WHERE NULL) t WHERE NULL;
SELECT t.c3 + 1 FROM (SELECT 1/0 AS c3 FROM a68f WHERE false) t WHERE false;
SELECT 1 FROM (SELECT 1/0 AS c3 FROM a68f WHERE false) t WHERE false;
SELECT t.id FROM (SELECT id, 1/0 AS c3 FROM a68f WHERE false) t WHERE false;
SELECT 1, 1, mode() WITHIN GROUP (ORDER BY 1) FROM a68fv t1 WHERE NULL GROUP BY CUBE (t1.c3, 1);
INSERT INTO a68f (id, a) SELECT 1, mode() WITHIN GROUP (ORDER BY 1) FROM a68fv t1 WHERE NULL GROUP BY CUBE (t1.c3, 1);
SELECT count(*) FROM a68f;
SELECT count(*) FROM a68fv t1 WHERE NULL;
SELECT t.c1 FROM a68fv t WHERE NULL;
SELECT x.id FROM a68f x LEFT JOIN (SELECT 1/0 AS c3 FROM a68f WHERE false) t ON t.c3 = x.id WHERE false;
SELECT x.id FROM a68f x LEFT JOIN (SELECT 1/0 AS c3 FROM a68f WHERE false) t ON false;
SELECT t.c3 FROM a68f x LEFT JOIN (SELECT 1/0 AS c3 FROM a68f WHERE false) t ON true;
DROP VIEW a68fv;
DROP TABLE a68f, a68f0;
