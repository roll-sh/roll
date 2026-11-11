-- eval_const_expressions: a strict function over a NULL constant is NULL (a custom plan's
-- NULL parameter too), COALESCE drops NULL constants and ends at a constant; VERBOSE row
-- marks of locked rows (a FROM subquery's FOR UPDATE under its Subquery Scan).
CREATE TABLE a3_cf (two int, four int, f text);
INSERT INTO a3_cf VALUES (1, 2, 'a'), (2, 3, NULL);
EXPLAIN (COSTS OFF) SELECT * FROM a3_cf WHERE two = NULL;
SELECT * FROM a3_cf WHERE two = NULL;
EXPLAIN (COSTS OFF) SELECT * FROM a3_cf WHERE (two * 2) / NULL::int = 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT two + NULL, f || NULL, coalesce(NULL::int, 1), coalesce(NULL::int, two, 3, four), concat(f, NULL) FROM a3_cf;
SELECT two + NULL, f || NULL, coalesce(NULL::int, 1), coalesce(NULL::int, two, 3, four), concat(f, NULL) FROM a3_cf;
PREPARE a3_cfp(int) AS SELECT * FROM a3_cf WHERE two / $1 = 1;
EXPLAIN (COSTS OFF) EXECUTE a3_cfp(NULL);
EXECUTE a3_cfp(NULL);
EXECUTE a3_cfp(1);
DEALLOCATE a3_cfp;
CREATE VIEW a3_cfv AS SELECT two + NULL AS x, coalesce(NULL::int, two) AS y, f FROM a3_cf WHERE four = NULL;
SELECT pg_get_viewdef('a3_cfv');
SELECT NULL + 1 / 0;
EXPLAIN (VERBOSE, COSTS OFF)
WITH x AS (SELECT * FROM (SELECT two FROM a3_cf FOR UPDATE) ss)
SELECT * FROM x WHERE two = 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT two FROM a3_cf FOR UPDATE) ss WHERE two = 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT * FROM a3_cf FOR SHARE) ss;
EXPLAIN (VERBOSE, COSTS OFF) SELECT two FROM a3_cf FOR UPDATE;
SELECT * FROM (SELECT two FROM a3_cf FOR UPDATE) ss WHERE two = 1;
DROP VIEW a3_cfv;
DROP TABLE a3_cf;
