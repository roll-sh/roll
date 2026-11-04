-- Zero costs select the PostgreSQL reference shape. The engine uses fixed CPU costs.
-- The Rust contract test controls build-first execution on a clone of the engine plan.
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
SELECT bool_or(c.relname = 'a166_keys_inside') AS seen_inside
FROM pg_class c JOIN (SELECT a166_make_keys() AS ns OFFSET 0) f ON c.relnamespace = f.ns
WHERE c.relkind IN ('r', 'v');
SELECT count(*) AS seen_next_statement FROM pg_class WHERE relname = 'a166_keys_inside';
CREATE TABLE a166_count_before (n bigint);
INSERT INTO a166_count_before SELECT count(*) FROM pg_class;
CREATE FUNCTION a166_make_count() RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CREATE TABLE a166_count_inside (id int);
  RETURN (SELECT n FROM a166_count_before);
END
$$;
SELECT count(*) AS seen_old_count
FROM (SELECT count(*) AS n FROM pg_class) c
WHERE c.n IN (SELECT a166_make_count());
SELECT (SELECT count(*) FROM pg_class) = (SELECT n + 1 FROM a166_count_before) AS one_more_next_statement;
DROP TABLE a166_count_inside, a166_count_before, a166_keys_inside, a166_keys_anchor;
DROP FUNCTION a166_make_count(), a166_make_keys();
