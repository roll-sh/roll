-- SP-GiST quad_point_ops over a point with a NaN coordinate (agent 114): getQuadrant
-- places it in no quadrant, which PostgreSQL meets when the root leaf page (226 points)
-- splits with it on it, or when one is inserted once the root has split. kd_point_ops
-- doesn't fail.
CREATE TABLE a114_p (p point);
INSERT INTO a114_p VALUES ('(NaN,1)');
CREATE INDEX a114_p_q ON a114_p USING spgist (p);
INSERT INTO a114_p SELECT point(g, g) FROM generate_series(1, 225) g;
SELECT count(*) FROM a114_p;
INSERT INTO a114_p VALUES ('(1,1)');
SELECT count(*) FROM a114_p;
DROP INDEX a114_p_q;
INSERT INTO a114_p VALUES (NULL), (NULL);
CREATE INDEX a114_p_q ON a114_p USING spgist (p);
SELECT count(*) FROM a114_p WHERE p IS NULL;
DELETE FROM a114_p WHERE p ~= '(225,225)';
CREATE INDEX a114_p_q2 ON a114_p USING spgist (p quad_point_ops);
CREATE INDEX a114_p_kd ON a114_p USING spgist (p kd_point_ops);
TRUNCATE a114_p;
INSERT INTO a114_p SELECT point(g, g) FROM generate_series(1, 225) g;
INSERT INTO a114_p VALUES ('(NaN,NaN)');
CREATE INDEX a114_p_q3 ON a114_p USING spgist (p);
INSERT INTO a114_p VALUES ('(5,NaN)');
INSERT INTO a114_p VALUES ('(5,5)');
SELECT count(*) FROM a114_p;
-- Built over more than a page of points: the root splits with the NaN on it.
CREATE TABLE a114_p2 (p point);
INSERT INTO a114_p2 SELECT point(g, g) FROM generate_series(1, 300) g;
INSERT INTO a114_p2 VALUES ('(NaN,1)');
CREATE INDEX ON a114_p2 USING spgist (p);
CREATE INDEX ON a114_p2 USING spgist (p kd_point_ops);
DELETE FROM a114_p2 WHERE p[0] <> p[0];
CREATE INDEX a114_p2_q ON a114_p2 USING spgist (p);
INSERT INTO a114_p2 VALUES ('(NaN,1)');
INSERT INTO a114_p2 VALUES ('(1,2)');
BEGIN;
INSERT INTO a114_p2 VALUES ('(NaN,1)');
ROLLBACK;
UPDATE a114_p2 SET p = '(1,NaN)' WHERE p ~= '(1,1)';
SELECT count(*) FROM a114_p2;
DROP TABLE a114_p, a114_p2;
