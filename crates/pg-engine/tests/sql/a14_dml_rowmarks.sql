-- Round 16 (agent 14): EXPLAIN VERBOSE target lists of UPDATE / DELETE over joins. The
-- other relations' row marks: a table's ctid, a CTE's, VALUES' or a function's whole row
-- (ROW_MARK_COPY); an inheritance tree's row identity (tableoid, ctid) after them, each
-- member's scan returning it last; a target's scan reading its ctid builds no physical
-- target list (use_physical_tlist refuses system columns), ONLY a parent with children too.
CREATE TABLE a14_p (aa int, b text);
CREATE TABLE a14_c (cc int) INHERITS (a14_p);
CREATE TABLE a14_q (aa int, b text);
CREATE TABLE a14_i (q1 int8, q2 int8);
EXPLAIN (VERBOSE, COSTS OFF)
WITH wcte AS (INSERT INTO a14_i VALUES (42, 47) RETURNING q2)
DELETE FROM a14_p USING wcte WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
WITH wcte AS (INSERT INTO a14_i VALUES (42, 47) RETURNING q2)
DELETE FROM ONLY a14_p USING wcte WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
WITH wcte AS (INSERT INTO a14_i VALUES (42, 47) RETURNING q2)
UPDATE a14_p SET b = 'x' FROM wcte WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
DELETE FROM a14_p USING a14_i WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE a14_p SET b = q1::text FROM a14_i WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
DELETE FROM a14_p USING (VALUES (1), (2)) v(x) WHERE aa = x;
EXPLAIN (VERBOSE, COSTS OFF)
WITH wcte AS (INSERT INTO a14_i VALUES (42, 47) RETURNING q2)
DELETE FROM a14_q USING wcte WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
DELETE FROM a14_q USING a14_i WHERE aa = q2;
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE a14_q SET b = 'x' FROM (VALUES (1), (2)) v(x) WHERE aa = x;
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE a14_q SET b = 'y' FROM generate_series(1, 3) g WHERE aa = g;
INSERT INTO a14_p VALUES (47, 'p');
INSERT INTO a14_c VALUES (47, 'c', 1);
WITH wcte AS (INSERT INTO a14_i VALUES (42, 47) RETURNING q2)
DELETE FROM a14_p USING wcte WHERE aa = q2 RETURNING a14_p.*;
SELECT * FROM a14_p;
DROP TABLE a14_c, a14_p, a14_q, a14_i;
