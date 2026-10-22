-- `ROW(...) op (SELECT ...)` (ROWCOMPARE_SUBLINK): results, errors, view deparse and
-- EXPLAIN (a correlated one a SubPlan testing the comparison, an uncorrelated one an
-- InitPlan whose columns are separate quals); a gating Result's scan returns its physical
-- target list; a ROW() of constants is a record constant in EXPLAIN.
CREATE TABLE a3_rc (f1 int, f2 int, f3 float8);
INSERT INTO a3_rc VALUES (1, 2, 3), (2, 3, 4), (3, 4, 5), (1, 1, 1), (2, 2, 2), (6, 7, 8), (8, 9, NULL);
SELECT f1, f2, ROW(1, 2) = (SELECT f1, f2) AS eq, ROW(1, 2) <> (SELECT f1, f2) AS ne,
       ROW(1, 2) < (SELECT f1, f2) AS lt, ROW(2, 2) >= (SELECT f1, f2) AS ge
FROM a3_rc ORDER BY 1, 2;
SELECT ROW(1, 2) = (SELECT f1, f2 FROM a3_rc);
SELECT ROW(1, 2) = (SELECT f1, f2 FROM a3_rc WHERE false);
SELECT ROW(1, 2) = (SELECT f1 FROM a3_rc WHERE false);
SELECT ROW(1, 2) = (SELECT f1, f2, f3 FROM a3_rc WHERE false);
SELECT ROW(1, 2) = (SELECT 1, 2 UNION ALL SELECT 1, 3);
SELECT ROW(1, 2) <> (SELECT * FROM (VALUES (1, 3)) v);
SELECT ROW(1, 2) <= (SELECT f1, f2 FROM a3_rc ORDER BY 1, 2 LIMIT 1);
SELECT ROW(1) = (SELECT 1);
SELECT count(*) FROM a3_rc s WHERE ROW(s.f1, s.f2) = (SELECT t.f1, t.f2 FROM a3_rc t WHERE t.f1 = s.f1 AND t.f2 = s.f2);
SELECT f1 FROM a3_rc s WHERE ROW(1, 1) = (SELECT t.f1, t.f2 FROM a3_rc t WHERE t.f1 = s.f1 ORDER BY t.f2 LIMIT 1) ORDER BY 1;
CREATE VIEW a3_rc_v AS SELECT ROW(1, 2) = (SELECT f1, f2) AS eq, ROW(1, 2) < (SELECT f1, f2) AS lt FROM a3_rc;
SELECT pg_get_viewdef('a3_rc_v');
SELECT * FROM a3_rc_v;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ROW(1, 2) = (SELECT f1, f2) AS eq FROM a3_rc;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ROW(1, 2) = (SELECT 3, 4) AS eq FROM a3_rc;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ROW(1, 2) < (SELECT f1, f2) AS lt FROM a3_rc;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ROW(1, 2) <> (SELECT f1, f2) AS ne FROM a3_rc;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a3_rc WHERE ROW(f1, 2) = (SELECT 3, 4);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a3_rc WHERE ROW(f1, 2) = (SELECT f2, 4) AND f3 > 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT ROW(1, 2) = (SELECT f1, f2 FROM a3_rc s2 WHERE s2.f1 = s.f2 LIMIT 1) AS eq FROM a3_rc s;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a3_rc WHERE f1 = (SELECT 1) AND 2 = (SELECT 3);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT f1 FROM a3_rc WHERE 2 = (SELECT 3);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT row(row(row(1))) = any (SELECT row(row(1)));
EXPLAIN (VERBOSE, COSTS OFF)
SELECT row(row(1)), row(1, 'a'), row(row(1), 2);
DROP VIEW a3_rc_v;
DROP TABLE a3_rc;
