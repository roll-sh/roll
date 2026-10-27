-- Round 4 difftest finds, batch 1 (agent 84).
-- A sub-select is planned while its expression is preprocessed (make_subplan), whatever
-- later becomes of the relation reading it: its failing constants fail the statement even
-- on the nullable side of LEFT JOIN ... ON false (seed 2133 #66).
CREATE TABLE a84t1 (id int4 PRIMARY KEY, d numeric);
CREATE VIEW a84v1 AS SELECT 0.001 AS c0 FROM a84t1 WHERE 1 IN (SELECT 1 FROM a84t1 WHERE jsonb_array_length('null') <> 1);
SELECT 1 FROM (VALUES (1)) s LEFT JOIN a84v1 t2 ON false;
SELECT 1 FROM (VALUES (1)) s LEFT JOIN (SELECT 1 FROM a84t1 WHERE 1 IN (SELECT 1 FROM (SELECT 1) x WHERE jsonb_array_length('null') <> 1)) t2 ON 1 IS NULL;
SELECT 1 FROM (VALUES (1)) s LEFT JOIN (SELECT 1 FROM a84t1 WHERE false AND 1 IN (SELECT 1 FROM (SELECT 1) x WHERE jsonb_array_length('null') <> 1)) t2 ON false;
-- A constant-false WHERE over an outer join gates the whole join (the top join domain's
-- pseudoconstant), so a subquery planned apart on its outer side is still planned (seed
-- 2184 #1).
SELECT 1 FROM ((SELECT DISTINCT 1 AS c0, width_bucket('NaN', 0, 10, 4) AS c1 FROM (VALUES (1)) s2) s1 LEFT JOIN a84t1 ON true) WHERE false;
SELECT count(*) FROM ((SELECT DISTINCT 1 AS c0, width_bucket('NaN', 0, 10, 4) AS c1 FROM (VALUES (1)) s2) s1 LEFT JOIN a84t1 ON false) WHERE false;
SELECT 1 FROM (SELECT DISTINCT 1 AS c0, width_bucket('NaN', 0, 10, 4) AS c1 FROM (VALUES (1)) s2) s1 WHERE false;
SELECT count(*) FROM (a84t1 RIGHT JOIN (SELECT DISTINCT 1 AS c0, width_bucket('NaN', 0, 10, 4) AS c1) s1 ON true) WHERE NULL;
-- check_generic_type_consistency: an anyelement argument of a domain stays the domain,
-- anyarray / anyrange ones are flattened, so they don't match.
CREATE DOMAIN a84d AS int CHECK (value > -1000);
CREATE TABLE a84td (id int, x a84d, s text);
INSERT INTO a84td VALUES (1, 3, 'a'), (2, 5, 'b');
SELECT int4range(1, 10) @> x FROM a84td;
SELECT x <@ int4range(1, 10) FROM a84td;
SELECT int4range(1, 10) @> x::int FROM a84td ORDER BY id;
SELECT ARRAY[1, 2] @> ARRAY[x] FROM a84td;
SELECT array_append(ARRAY[x], 1), array_position(ARRAY[x], 3) FROM a84td ORDER BY id;
-- A sub-select of a domain column (or ARRAY() of one) has the domain's type.
SELECT (SELECT array_agg(x ORDER BY x) FROM a84td) AS a, (SELECT x FROM a84td ORDER BY id LIMIT 1) AS b, ARRAY(SELECT x FROM a84td ORDER BY id) AS c;
SELECT pg_typeof((SELECT array_agg(x) FROM a84td)), pg_typeof((SELECT x FROM a84td LIMIT 1)), pg_typeof(ARRAY(SELECT x FROM a84td));
-- record_cmp: a field whose two row types declare different collations has none, and
-- comparing two values of it fails; NULLs and fields after a difference never compare it.
SELECT x < y FROM (SELECT ROW('a'::text COLLATE "C", 1) x, ROW('B'::text, 1) y) s;
SELECT x = y FROM (SELECT ROW('a'::text COLLATE "C", 1) x, ROW('a'::text COLLATE "POSIX", 1) y OFFSET 0) s;
SELECT x = y FROM (SELECT ROW('a'::text COLLATE "C", 1) x, ROW('a'::text COLLATE "C", 2) y OFFSET 0) s;
SELECT x IS DISTINCT FROM y FROM (SELECT ROW('a'::text COLLATE "C", 1) x, ROW('a'::text, 1) y OFFSET 0) s;
SELECT x = y FROM (SELECT ROW(NULL::text COLLATE "C", 1) x, ROW('b'::text, 1) y OFFSET 0) s;
SELECT x < y FROM (SELECT ROW(1, s COLLATE "C") x, ROW(id, s) y FROM a84td) s;
SELECT x < y FROM (SELECT ROW(id, s COLLATE "C") x, ROW(3, s) y FROM a84td) s;
SELECT x IN (y) FROM (SELECT ROW(s COLLATE "C", 1) x, ROW(s, 1) y FROM a84td) s;
SELECT ROW('a'::text COLLATE "C", 1) < ROW('B'::text, 1);
-- coerce_record_to_complex: only a ROW() or a whole-row reference makes a record a
-- composite.
CREATE TABLE a84tc (id int, c a84t1);
SELECT max(r)::a84t1 FROM (SELECT ROW(1, 2)::a84t1 r) s;
SELECT r::a84t1 FROM (SELECT ROW(1, 2) r) s;
SELECT (SELECT ROW(1, 2))::a84t1;
SELECT s::a84t1, ROW(1, 2)::a84t1 FROM (SELECT 1 a, 2 b) s;
WITH q AS (SELECT 1 a, 2 b) SELECT q::a84t1 FROM q;
INSERT INTO a84tc (c) SELECT ROW(1, 2);
INSERT INTO a84tc (id, c) SELECT 1, ROW(1, 2) UNION ALL SELECT 2, ROW(3, 4);
INSERT INTO a84tc (c) VALUES (ROW(1, 2)), (ROW(3, 4));
UPDATE a84tc SET c = (SELECT ROW(5, 6));
UPDATE a84tc SET (c) = (SELECT ROW(5, 6));
UPDATE a84tc SET c = ROW(5, 6);
SELECT * FROM a84tc;
DROP TABLE a84tc;
