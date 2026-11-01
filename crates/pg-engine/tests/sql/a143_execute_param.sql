-- EXECUTE's parameters (agent 143): expressions of EXPR_KIND_EXECUTE_PARAMETER, where
-- a subquery, an aggregate, a window function or a set-returning function is refused.
PREPARE a143_p(int) AS SELECT $1 + 1 AS r;
EXECUTE a143_p(1);
EXECUTE a143_p((SELECT 2));
EXECUTE a143_p(1 + (SELECT 2));
EXECUTE a143_p(EXISTS (SELECT 1)::int);
EXECUTE a143_p(count(*));
EXECUTE a143_p(row_number() OVER ());
EXECUTE a143_p(generate_series(1, 2));
EXECUTE a143_p(a143_nosuch);
EXPLAIN (COSTS OFF) EXECUTE a143_p((SELECT 2));
EXPLAIN (COSTS OFF) EXECUTE a143_p(sum(1));
CREATE TEMP TABLE a143_t AS EXECUTE a143_p((SELECT 3));
CREATE TEMP TABLE a143_t AS EXECUTE a143_p(3);
SELECT * FROM a143_t;
DROP TABLE a143_t;
DEALLOCATE a143_p;
