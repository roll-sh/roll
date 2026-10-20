-- remove_unused_subquery_outputs (group C1): a view's or subquery's output columns the
-- outer query never reads aren't computed (1/0 there raises nothing), unless volatile,
-- set-returning, sorted / grouped / DISTINCT on, or under a set operation that compares
-- whole rows.
CREATE TABLE gcu_t (a int, b int);
INSERT INTO gcu_t VALUES (0, 5);
CREATE VIEW gcu_v AS SELECT 1 / a AS x, b AS y FROM gcu_t;
SELECT y FROM gcu_v;
SELECT count(*) FROM gcu_v;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t OFFSET 0) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t GROUP BY a, b) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t ORDER BY b) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t LIMIT 5) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t FOR UPDATE) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y, (SELECT 1 / a) AS z FROM gcu_t) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t UNION ALL SELECT 1, 2) s ORDER BY y;
SELECT y FROM (SELECT DISTINCT ON (b) 1 / a AS x, b AS y FROM gcu_t) s;
SELECT s.y FROM gcu_t JOIN LATERAL (SELECT 1 / gcu_t.a AS x, gcu_t.b AS y OFFSET 0) s ON true;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t) s WHERE y > 0 ORDER BY y;
SELECT y FROM (SELECT sum(1 / a) AS x, b AS y FROM gcu_t GROUP BY b) s;
SELECT y FROM (SELECT sum(1 / a) AS x, 5 AS y FROM gcu_t) s;
SELECT y FROM (SELECT sum(1 / a) AS x, count(*) AS y FROM gcu_t) s;
SELECT y FROM (SELECT sum(1 / a) FILTER (WHERE b > 0) AS x, max(b) AS y FROM gcu_t) s;
SELECT g, y FROM (SELECT grouping(b) AS g, sum(1 / a) AS x, b AS y FROM gcu_t
  GROUP BY GROUPING SETS ((b), ())) s ORDER BY g;
SELECT y FROM (SELECT json_object_agg(NULL::text, 1) AS x, b AS y FROM gcu_t GROUP BY b) s;
SELECT y FROM (SELECT 1 / a AS x, row_number() OVER () AS y FROM gcu_t) s;
SELECT y FROM (SELECT 1 / a AS x, generate_series(1, 2) AS y FROM gcu_t) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT y FROM gcu_v;
-- Still computed: the error stands.
SELECT y FROM (SELECT DISTINCT 1 / a AS x, b AS y FROM gcu_t) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t UNION SELECT 1, 2) s;
SELECT y FROM (SELECT 1 / a AS x, b AS y FROM gcu_t ORDER BY 1 / a) s;
SELECT y FROM (SELECT generate_series(1, 2) / a AS x, b AS y FROM gcu_t) s;
WITH w AS MATERIALIZED (SELECT 1 / a AS x, b AS y FROM gcu_t) SELECT y FROM w;
SELECT y FROM (SELECT 1 / (a + (0 * random())::int) AS x, b AS y FROM gcu_t) s;
