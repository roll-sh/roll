-- Planner and EXPLAIN shapes (group C1), part 3: partitionwise aggregation, window run
-- conditions and frames, debug_parallel_query, transition tables and PL functions in
-- FROM, MERGE under EXPLAIN ANALYZE, one-row VALUES pulled up, inherited statistics.
CREATE TABLE gc3_p (a int, b int, c text) PARTITION BY LIST (a);
CREATE TABLE gc3_p1 PARTITION OF gc3_p FOR VALUES IN (0, 1, 2);
CREATE TABLE gc3_p2 PARTITION OF gc3_p FOR VALUES IN (3, 4, 5);
CREATE TABLE gc3_p3 PARTITION OF gc3_p FOR VALUES IN (6, 7, 8, 9);
INSERT INTO gc3_p SELECT i % 10, i % 7, to_char(i % 4, 'FM0000') FROM generate_series(1, 3000) i;
ANALYZE gc3_p;
SET enable_partitionwise_aggregate = on;
EXPLAIN (COSTS OFF) SELECT a, sum(b), count(*) FROM gc3_p GROUP BY a HAVING avg(b) < 3.2 ORDER BY 1;
SELECT a, sum(b), count(*) FROM gc3_p GROUP BY a HAVING avg(b) < 3.2 ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT b, sum(a), count(*) FROM gc3_p GROUP BY b ORDER BY 1;
SELECT b, sum(a), count(*) FROM gc3_p GROUP BY b ORDER BY 1;
RESET enable_partitionwise_aggregate;
-- Run conditions and frames of window functions.
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT a, b, row_number() OVER (ORDER BY a, b) rn FROM gc3_p1) s WHERE rn <= 3;
SELECT * FROM (SELECT a, b, row_number() OVER (ORDER BY a, b) rn FROM gc3_p1) s WHERE rn <= 3;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT a, b, count(*) OVER (PARTITION BY a ORDER BY b) c FROM gc3_p1) s WHERE c <= 2;
SELECT * FROM (SELECT a, b, count(*) OVER (PARTITION BY a ORDER BY b) c FROM gc3_p1) s WHERE c <= 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, sum(b) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), rank() OVER (ORDER BY a) FROM gc3_p2 LIMIT 3;
-- A parallel-safe query under debug_parallel_query: a single-copy Gather.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc3_p1 WHERE b > 2;
RESET debug_parallel_query;
-- A trigger's transition table and a PL/pgSQL function in FROM.
CREATE TABLE gc3_t (id int, val text);
CREATE FUNCTION gc3_t_ins() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE t text := ''; l text;
BEGIN
  FOR l IN EXECUTE 'EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM nt' LOOP
    t := t || l || E'\n';
  END LOOP;
  RAISE INFO '%', t;
  RETURN NULL;
END $$;
CREATE TRIGGER gc3_t_ins AFTER INSERT ON gc3_t REFERENCING NEW TABLE AS nt
  FOR EACH STATEMENT EXECUTE FUNCTION gc3_t_ins();
INSERT INTO gc3_t VALUES (1, 'one'), (2, 'two');
CREATE FUNCTION gc3_arr(int) RETURNS int[] LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN array[$1, $1 + 1]; END $$;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM gc3_arr(4) x;
SELECT x FROM gc3_arr(4) x;
-- MERGE: the target first on equal paths, and what happened to the rows.
CREATE TABLE gc3_mt (a int, b int);
CREATE TABLE gc3_ms (a int, b int);
INSERT INTO gc3_mt SELECT i, i * 10 FROM generate_series(1, 100, 2) i;
INSERT INTO gc3_ms SELECT i, i * 10 FROM generate_series(1, 100) i;
CREATE FUNCTION gc3_explain(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE ln text;
BEGIN
  FOR ln IN EXECUTE 'EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF, BUFFERS OFF) ' || q LOOP
    ln := regexp_replace(ln, 'Memory: \S*', 'Memory: xxx', 'g');
    RETURN NEXT ln;
  END LOOP;
END $$;
SELECT gc3_explain('MERGE INTO gc3_mt t USING gc3_ms s ON t.a = s.a WHEN MATCHED AND t.a < 10 THEN UPDATE SET b = t.b + 1');
SELECT gc3_explain('MERGE INTO gc3_mt t USING gc3_ms s ON t.a = s.a AND t.a < -1000 WHEN MATCHED THEN DELETE');
SELECT gc3_explain('MERGE INTO gc3_mt t USING gc3_ms s ON t.a = s.a WHEN NOT MATCHED BY SOURCE AND t.a < 10 THEN DELETE WHEN NOT MATCHED BY TARGET AND s.a < 20 THEN INSERT VALUES (a, b)');
SELECT count(*), sum(b) FROM gc3_mt;
-- A one-row VALUES list is its constants.
EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (1, 10)) v(x, y) LEFT JOIN gc3_mt t ON t.a = v.x AND t.b = v.y;
EXPLAIN (COSTS OFF) MERGE INTO gc3_mt t USING (VALUES (3, 5)) v(x, y) ON t.a = v.x WHEN NOT MATCHED THEN INSERT VALUES (x, y);
-- Inherited statistics of a plain inheritance tree.
CREATE TABLE gc3_i (a int, b int);
CREATE TABLE gc3_i1 () INHERITS (gc3_i);
INSERT INTO gc3_i SELECT i % 50, i % 100 FROM generate_series(0, 1999) i;
INSERT INTO gc3_i1 SELECT i % 100, i % 100 FROM generate_series(0, 999) i;
CREATE STATISTICS gc3_i_s ON a, b FROM gc3_i;
ANALYZE gc3_i, gc3_i1;
EXPLAIN SELECT a, b FROM gc3_i GROUP BY 1, 2;
