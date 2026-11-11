-- Inner and semi merge joins end when the inner side runs out (agent 143), as
-- ExecMergeJoin does (MJ_ENDINNER), or at an outer row whose first key is a null sorting
-- last (MJEVAL_ENDOFJOIN): the outer rows past it are never read (memoize's first plan
-- without create_index's indexes read all 10,000 rows of its outer Sort, against 21).
CREATE TABLE a143_m1 AS SELECT g AS u, g % 20 AS t FROM generate_series(0, 9999) g;
CREATE TABLE a143_m2 (k int, v int);
INSERT INTO a143_m2 SELECT g % 30, g FROM generate_series(1, 300) g;
INSERT INTO a143_m2 VALUES (NULL, 1), (NULL, 2);
ANALYZE a143_m1, a143_m2;
CREATE FUNCTION a143_explain(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE ln text;
BEGIN
  FOR ln IN EXECUTE format('EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF, BUFFERS OFF) %s', q) LOOP
    ln := regexp_replace(ln, 'Memory: \d+kB', 'Memory: NkB');
    ln := regexp_replace(ln, 'Sort Method: \S+', 'Sort Method: S');
    RETURN NEXT ln;
  END LOOP;
END $$;
SET enable_hashjoin = off;
SET enable_nestloop = off;
SET enable_indexscan = off;
SET enable_bitmapscan = off;
SET max_parallel_workers_per_gather = 0;
SELECT a143_explain('SELECT count(*), avg(a.u) FROM a143_m1 a JOIN a143_m1 b ON a.u = b.t WHERE b.u < 1000');
SELECT count(*), avg(a.u) FROM a143_m1 a JOIN a143_m1 b ON a.u = b.t WHERE b.u < 1000;
-- a semi join
SELECT a143_explain('SELECT count(*) FROM a143_m1 a WHERE a.u IN (SELECT k FROM a143_m2)');
SELECT count(*) FROM a143_m1 a WHERE a.u IN (SELECT k FROM a143_m2);
-- nulls on the outer side, sorting last: the join ends at the first
SELECT count(*) FROM a143_m2 x JOIN (SELECT t FROM a143_m1 WHERE u < 500) y ON x.k = y.t;
-- a left join reads its outer side all the same
SELECT count(*) FROM a143_m1 a LEFT JOIN a143_m2 b ON a.u = b.k;
-- descending keys (the inner side's reads, which a streaming merge join would stop
-- early too, still differ under EXPLAIN ANALYZE: not shown)
SELECT a.u, b.v FROM a143_m1 a JOIN a143_m2 b ON a.u = b.k ORDER BY a.u DESC, b.v LIMIT 3;
-- Ordered subqueries force descending merge inputs, with nulls at either end.
SELECT sum(a.u), count(*) FROM
  (SELECT u FROM a143_m1 ORDER BY u DESC NULLS FIRST) a JOIN
  (SELECT k FROM a143_m2 ORDER BY k DESC NULLS FIRST) b ON a.u = b.k;
SELECT sum(a.u), count(*) FROM
  (SELECT u FROM a143_m1 ORDER BY u DESC NULLS LAST) a JOIN
  (SELECT k FROM a143_m2 ORDER BY k DESC NULLS LAST) b ON a.u = b.k;
RESET enable_hashjoin;
RESET enable_nestloop;
RESET enable_indexscan;
RESET enable_bitmapscan;
RESET max_parallel_workers_per_gather;
DROP FUNCTION a143_explain(text);
DROP TABLE a143_m1, a143_m2;
