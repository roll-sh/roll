-- A system catalog inside a join: its indexes give parameterized index scans (the
-- catalog's stand-in table's paths). A LATERAL subquery's scan with a clause over its
-- PlaceHolderVar (a restriction there) and a join clause: the restriction first, the join
-- clause's value the first cache key.
CREATE TABLE a8_cl (a int, b text);
EXPLAIN (COSTS OFF)
SELECT tname, attname FROM (
SELECT relname::information_schema.sql_identifier AS tname, * FROM
  (SELECT * FROM pg_class c) ss1) ss2
  RIGHT JOIN pg_attribute a ON a.attrelid = ss2.oid
WHERE tname = 'a8_cl' AND attnum = 1;
SELECT tname, attname FROM (
SELECT relname::information_schema.sql_identifier AS tname, * FROM
  (SELECT * FROM pg_class c) ss1) ss2
  RIGHT JOIN pg_attribute a ON a.attrelid = ss2.oid
WHERE tname = 'a8_cl' AND attnum = 1;
CREATE TABLE a8_tk (unique1 int, two int, twenty int);
INSERT INTO a8_tk SELECT i, i % 2, i % 20 FROM generate_series(0, 9999) i;
CREATE INDEX a8_tk_u1 ON a8_tk (unique1);
ANALYZE a8_tk;
EXPLAIN (COSTS OFF)
SELECT COUNT(*), AVG(t1.twenty) FROM a8_tk t1 LEFT JOIN
LATERAL (SELECT t1.twenty AS c1, t2.unique1 AS c2, t2.two FROM a8_tk t2) s
ON t1.two = s.two
WHERE s.c1 = s.c2 AND t1.unique1 < 1000;
SELECT COUNT(*), AVG(t1.twenty) FROM a8_tk t1 LEFT JOIN
LATERAL (SELECT t1.twenty AS c1, t2.unique1 AS c2, t2.two FROM a8_tk t2) s
ON t1.two = s.two
WHERE s.c1 = s.c2 AND t1.unique1 < 1000;
SELECT COUNT(*), AVG(t1.twenty) FROM a8_tk t1 LEFT JOIN
LATERAL (SELECT t1.two + 1 AS c1, t2.unique1 AS c2 FROM a8_tk t2) s ON TRUE
WHERE s.c1 = s.c2 AND t1.unique1 < 1000;
DROP TABLE a8_tk;
DROP TABLE a8_cl;
