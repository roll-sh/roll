-- Cost-driven choices: a UNION ALL arm under a LIMIT planned for the rows it reads, an
-- appendrel's parameterized index scans on an expression index, an expression index's
-- statistics in join selectivity (only for a user who may read the whole table), a
-- pattern over an expression without statistics, MIN / MAX through an index under
-- GROUP BY (), and grouping sets over input an index already orders.
CREATE TABLE gc1_a (unique1 int, unique2 int, thousand int, tenthous int);
INSERT INTO gc1_a SELECT g, (g * 7) % 10000, g % 1000, g FROM generate_series(0, 9999) g;
CREATE INDEX gc1_a_tt ON gc1_a (thousand, tenthous);
CREATE INDEX gc1_a_u1 ON gc1_a (unique1);
CREATE TABLE gc1_b (unique1 int, thousand int, tenthous int);
INSERT INTO gc1_b SELECT g, g % 1000, g FROM generate_series(0, 9999) g;
ANALYZE gc1_a;
ANALYZE gc1_b;
EXPLAIN (COSTS OFF)
SELECT t1.unique1 FROM gc1_a t1 INNER JOIN gc1_b t2 ON t1.tenthous = t2.tenthous AND t2.thousand = 0
UNION ALL (VALUES (1)) LIMIT 1;
SELECT t1.unique1 FROM gc1_a t1 INNER JOIN gc1_b t2 ON t1.tenthous = t2.tenthous AND t2.thousand = 0
UNION ALL (VALUES (1)) LIMIT 1;

CREATE FUNCTION gc1_expensive(int) RETURNS int LANGUAGE plpgsql IMMUTABLE STRICT COST 10000
AS $$BEGIN RETURN $1; END$$;
CREATE TABLE gc1_t3 AS SELECT generate_series(-1000, 1000) AS x;
CREATE INDEX gc1_t3i ON gc1_t3 (gc1_expensive(x));
CREATE TABLE gc1_small (f1 int);
INSERT INTO gc1_small VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
ANALYZE gc1_t3;
ANALYZE gc1_small;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT * FROM gc1_t3 a UNION ALL SELECT * FROM gc1_t3 b) ss
  JOIN gc1_small ON f1 = gc1_expensive(x);
SELECT * FROM (SELECT * FROM gc1_t3 a UNION ALL SELECT * FROM gc1_t3 b) ss
  JOIN gc1_small ON f1 = gc1_expensive(x);
EXPLAIN SELECT * FROM gc1_t3 a JOIN gc1_small ON f1 = gc1_expensive(x);

CREATE TABLE gc1_pp (a int, b text, c text) PARTITION BY LIST (a);
CREATE TABLE gc1_pp1 PARTITION OF gc1_pp FOR VALUES IN (1);
CREATE INDEX ON gc1_pp (left(c, 3));
INSERT INTO gc1_pp SELECT 1, 'a', left(md5(i::text), 5) FROM generate_series(0, 100) i;
ANALYZE gc1_pp;
CREATE ROLE gc1_reader;
GRANT SELECT (a, c) ON gc1_pp TO gc1_reader;
SET SESSION AUTHORIZATION gc1_reader;
EXPLAIN (COSTS OFF)
SELECT p2.a, p1.c FROM gc1_pp p1 INNER JOIN gc1_pp p2 ON p1.a = p2.a AND left(p1.c, 3) ~ 'a1$';
RESET SESSION AUTHORIZATION;
REVOKE ALL ON gc1_pp FROM gc1_reader;
DROP ROLE gc1_reader;

EXPLAIN (COSTS OFF) SELECT min(unique1) FROM gc1_a GROUP BY ();
SELECT min(unique1) FROM gc1_a GROUP BY ();

CREATE TEMP TABLE gc1_gs (a int PRIMARY KEY, b int);
INSERT INTO gc1_gs VALUES (1, 1), (2, 2);
BEGIN;
SET LOCAL enable_hashagg = false;
SET LOCAL enable_seqscan = false;
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM gc1_gs GROUP BY GROUPING SETS (a, b, ()) ORDER BY a, b;
SELECT a, b, count(*) FROM gc1_gs GROUP BY GROUPING SETS (a, b, ()) ORDER BY a, b;
COMMIT;
