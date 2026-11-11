-- Round 23, agent 116: planner and EXPLAIN fixes found by comparing ORM, TPC-DS-style and
-- TPC-H plans with PostgreSQL 18.6 (tools/bench/plancmp.py).
CREATE TABLE a116_t (g int, n numeric, i int, f float8, t text);
INSERT INTO a116_t SELECT i % 100, i * 1.5, i, i / 3.0, 'x' || (i % 37) FROM generate_series(1, 20000) i;
ANALYZE a116_t;
-- An aggregate's final function runs once per group (get_agg_clause_costs' finalCost),
-- calls sharing a transition state transition once (sum and avg of numeric).
EXPLAIN SELECT g, sum(n) FROM a116_t GROUP BY g;
EXPLAIN SELECT sum(n), avg(i), count(*) FROM a116_t;
EXPLAIN SELECT g, avg(n), stddev(f) FROM a116_t GROUP BY g ORDER BY g;
EXPLAIN SELECT g, string_agg(t, ','), array_agg(i) FROM a116_t GROUP BY g;
EXPLAIN SELECT g, sum(i), sum(i::int8), max(n) FROM a116_t GROUP BY g;
EXPLAIN SELECT g, sum(n), avg(n) FROM a116_t GROUP BY g;
EXPLAIN SELECT g, count(i) FILTER (WHERE i > 10) FROM a116_t GROUP BY g;
-- A sub-select in LIMIT / OFFSET: an InitPlan of the level.
EXPLAIN SELECT * FROM a116_t LIMIT (SELECT 5);
EXPLAIN SELECT * FROM a116_t ORDER BY i OFFSET (SELECT 3) LIMIT (SELECT 5);
SELECT i FROM a116_t ORDER BY i OFFSET (SELECT 3) LIMIT (SELECT 2);
-- A LIMIT keeps its input's order: the query above a sorted LIMIT subquery doesn't sort
-- again (SQLAlchemy's limit/offset wrapper).
EXPLAIN SELECT s.i, s.n FROM (SELECT i, n FROM a116_t WHERE g = 3 ORDER BY n DESC LIMIT 10 OFFSET 0) s ORDER BY s.n DESC;
-- A grouped sub-select's outputs have no statistics; its one GROUP BY key is unique
-- (examine_simple_variable): IN over it joins by merging, no Memoize over its keys.
CREATE TABLE a116_o (k int PRIMARY KEY, c int, d date);
CREATE TABLE a116_l (k int, ln int, q numeric, PRIMARY KEY (k, ln));
INSERT INTO a116_o SELECT i, i % 997, date '1995-01-01' + i % 2000 FROM generate_series(1, 6000) i;
INSERT INTO a116_l SELECT i / 4 + 1, i % 4, (i * 7) % 50 FROM generate_series(0, 23999) i;
VACUUM ANALYZE a116_o, a116_l;
EXPLAIN SELECT k FROM a116_l GROUP BY k HAVING sum(q) > 150;
EXPLAIN SELECT o.k FROM a116_o o WHERE o.k IN (SELECT k FROM a116_l GROUP BY k HAVING sum(q) > 150);
SELECT count(*) FROM a116_o o WHERE o.k IN (SELECT k FROM a116_l GROUP BY k HAVING sum(q) > 150);
-- Parallel plans (forced at this size).
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
-- A FROM subquery with a written OFFSET 0 is no parallel relation to the query above
-- (set_rel_consider_parallel's limit_needed): it gathers its own plan (Prisma's count).
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT k FROM a116_l WHERE q > 3 OFFSET 0) sub;
SELECT count(*) FROM (SELECT k FROM a116_l WHERE q > 3 OFFSET 0) sub;
-- An outer join's clause over its nullable side is the partial join's too.
EXPLAIN (COSTS OFF) SELECT o.* FROM a116_o o LEFT JOIN a116_l l ON l.ln = o.c WHERE l.k IS NULL AND o.c < 50;
SELECT count(*) FROM a116_o o LEFT JOIN a116_l l ON l.ln = o.c WHERE l.k IS NULL AND o.c < 50;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET min_parallel_index_scan_size;
-- A target list's sub-select is computed by whichever path the LIMIT reads (Django's
-- subquery annotation).
EXPLAIN SELECT o.k, (SELECT max(l.q) FROM a116_l l WHERE l.k = o.k) FROM a116_o o WHERE o.c = 7 ORDER BY o.k LIMIT 5;
SELECT o.k, (SELECT max(l.q) FROM a116_l l WHERE l.k = o.k) FROM a116_o o WHERE o.c = 7 ORDER BY o.k LIMIT 5;
DROP TABLE a116_t, a116_o, a116_l;
