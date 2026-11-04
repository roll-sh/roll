CREATE FUNCTION a166_make_table() RETURNS oid LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CREATE TABLE a166_created_inside (id int);
  RETURN 2200;
END
$$;
SET enable_nestloop = off;
SET enable_mergejoin = off;
SET join_collapse_limit = 1;
EXPLAIN (COSTS OFF) SELECT bool_or(c.relname = 'a166_created_inside') AS seen_inside
FROM (SELECT relname, relnamespace FROM pg_class WHERE relkind = 'r' ORDER BY relname OFFSET 0) c JOIN (SELECT a166_make_table() AS ns OFFSET 0) f ON c.relnamespace = f.ns;
SELECT bool_or(c.relname = 'a166_created_inside') AS seen_inside
FROM (SELECT relname, relnamespace FROM pg_class WHERE relkind = 'r' ORDER BY relname OFFSET 0) c JOIN (SELECT a166_make_table() AS ns OFFSET 0) f ON c.relnamespace = f.ns;
SELECT count(*) AS seen_next_statement FROM pg_class WHERE relname = 'a166_created_inside';
SET a166.bad_oid = 'not_an_oid';
SELECT count(*) AS no_qualified_rows FROM pg_class WHERE relkind = 'x' AND relnamespace = current_setting('a166.bad_oid')::oid;
SELECT count(*) AS runtime_bad_oid FROM pg_class WHERE relnamespace = current_setting('a166.bad_oid')::oid;
