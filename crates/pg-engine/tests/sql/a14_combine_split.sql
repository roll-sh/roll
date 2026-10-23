-- Round 16 (agent 14): an aggregation planned as a Finalize Aggregate over partial ones
-- (Gather) runs the user-defined aggregates' combine functions over the partial state
-- (advance_combine_function's strictness rules), as the plan EXPLAIN shows says.
CREATE TABLE a14_cs AS SELECT i % 100 AS hundred, i % 4 AS four FROM generate_series(1, 10000) i;
ANALYZE a14_cs;
CREATE FUNCTION a14_balkifnull(int8, int8) RETURNS int8 PARALLEL SAFE STRICT LANGUAGE plpgsql AS $$
BEGIN
    IF $1 IS NULL THEN
       RAISE 'erroneously called with NULL argument';
    END IF;
    RETURN NULL;
END$$;
CREATE AGGREGATE a14_balk(int4) (
    SFUNC = int4_sum(int8, int4),
    STYPE = int8,
    COMBINEFUNC = a14_balkifnull(int8, int8),
    PARALLEL = SAFE,
    INITCOND = '0'
);
CREATE AGGREGATE a14_sum(int4) (SFUNC = int4_sum(int8, int4), STYPE = int8, COMBINEFUNC = int8pl, PARALLEL = SAFE);
ALTER TABLE a14_cs SET (parallel_workers = 4);
SET parallel_setup_cost = 0;
SET max_parallel_workers_per_gather = 4;
EXPLAIN (COSTS OFF) SELECT a14_balk(hundred) FROM a14_cs;
SELECT a14_balk(hundred) FROM a14_cs;
EXPLAIN (COSTS OFF) SELECT a14_balk(hundred), four FROM a14_cs GROUP BY four ORDER BY 2;
SELECT a14_balk(hundred), four FROM a14_cs GROUP BY four ORDER BY 2;
SELECT a14_sum(hundred) FROM a14_cs;
SELECT a14_sum(hundred) FROM a14_cs WHERE four > 5;
SET max_parallel_workers_per_gather = 0;
SELECT a14_balk(hundred) FROM a14_cs;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
DROP AGGREGATE a14_sum(int4);
DROP AGGREGATE a14_balk(int4);
DROP FUNCTION a14_balkifnull(int8, int8);
DROP TABLE a14_cs;
