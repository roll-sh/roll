-- A catalog scan opened before an inner volatile call keeps that catalog version,
-- including when a hash join builds the inner side before reading any outer row.
CREATE FUNCTION a166_make_relation() RETURNS oid LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CREATE TABLE a166_relation_inside (id int);
  RETURN 2200;
END
$$;
CREATE TABLE a166_anchor (id int);
SET enable_nestloop = off;
SET enable_mergejoin = off;
SET join_collapse_limit = 1;
SELECT bool_or(c.relname = 'a166_relation_inside') AS seen_inside
FROM (SELECT relname, relnamespace FROM pg_class WHERE relkind IN ('r', 'v') OFFSET 0) c
FULL JOIN (SELECT a166_make_relation() AS ns OFFSET 0) f ON c.relnamespace = f.ns;
SELECT count(*) AS seen_next_statement FROM pg_class WHERE relname = 'a166_relation_inside';
CREATE FUNCTION a166_make_attribute() RETURNS oid LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CREATE TABLE a166_attribute_inside (a166_new_column int);
  RETURN 0;
END
$$;
SELECT bool_or(a.attname = 'a166_new_column') AS seen_inside
FROM pg_attribute a FULL JOIN (SELECT a166_make_attribute() AS rel OFFSET 0) f ON a.attrelid = f.rel;
SELECT count(*) AS seen_next_statement FROM pg_attribute WHERE attname = 'a166_new_column';
DROP TABLE a166_attribute_inside, a166_relation_inside, a166_anchor;
DROP FUNCTION a166_make_attribute(), a166_make_relation();
