SET enable_nestloop = off;
SET enable_mergejoin = off;
SET enable_indexscan = off;
SET enable_bitmapscan = off;
SET max_parallel_workers_per_gather = 0;
SET join_collapse_limit = 1;
SET cpu_tuple_cost = 0;
SET cpu_operator_cost = 0;
SET seq_page_cost = 0;
SET random_page_cost = 0;
CREATE TABLE a166_keys_anchor (id int);
CREATE FUNCTION a166_make_keys() RETURNS oid LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CREATE TABLE a166_keys_inside (id int);
  RETURN 2200;
END
$$;
EXPLAIN (COSTS OFF) SELECT bool_or(c.relname = 'a166_keys_inside') AS seen_inside
FROM pg_class c JOIN (SELECT a166_make_keys() AS ns OFFSET 0) f ON c.relnamespace = f.ns
WHERE c.relkind IN ('r', 'v');
SELECT bool_or(c.relname = 'a166_keys_inside') AS seen_inside
FROM pg_class c JOIN (SELECT a166_make_keys() AS ns OFFSET 0) f ON c.relnamespace = f.ns
WHERE c.relkind IN ('r', 'v');
SELECT count(*) AS seen_next_statement FROM pg_class WHERE relname = 'a166_keys_inside';
CREATE TABLE a166_count_before (n bigint);
INSERT INTO a166_count_before SELECT count(*) FROM pg_class;
CREATE FUNCTION a166_make_count() RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CREATE TABLE a166_count_inside (id int);
  RETURN 0;
END
$$;
EXPLAIN (COSTS OFF) SELECT bool_and(c.n = (SELECT n FROM a166_count_before)) AS same_count
FROM (SELECT count(*) AS n FROM pg_class) c
FULL JOIN (SELECT a166_make_count() AS n OFFSET 0) f ON c.n = f.n;
SELECT bool_and(c.n = (SELECT n FROM a166_count_before)) AS same_count
FROM (SELECT count(*) AS n FROM pg_class) c
FULL JOIN (SELECT a166_make_count() AS n OFFSET 0) f ON c.n = f.n;
SELECT (SELECT count(*) FROM pg_class) = (SELECT n + 1 FROM a166_count_before) AS one_more_next_statement;
