-- class: error_detail
-- ordered: false
-- note: 42P01
-- engine: ERROR 42P01: invalid reference to FROM-clause entry for table "s1"
-- engine: DETAIL: There is an entry for table "s1", but it cannot be referenced from this part of the query.
-- engine: HINT: To reference that table, you must mark this subquery with LATERAL.
-- engine: POSITION: 213
-- reference: ERROR 42P01: invalid reference to FROM-clause entry for table "s1"
-- reference: DETAIL: There is an entry for table "s1", but it cannot be referenced from this part of the query.
-- reference: POSITION: 213

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b bool);

INSERT INTO t0 (id, a, b) VALUES (31, 6, true), (35, 6, true), (24, 0, false), (15, -2, NULL), (37, 6, true), (17, 0, true), (9, 0, true), (5, NULL, true), (29, 1, true), (14, 3, true), (38, 3, false), (18, 3, true);

CREATE INDEX t0_i870 ON t0 (id);

CREATE TABLE t1 (id int4, a int2, b int4, c date) PARTITION BY RANGE (b);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t1_p1 PARTITION OF t1 FOR VALUES FROM (3) TO (6);

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

INSERT INTO t1 (id, a, b, c) VALUES (40, 6, 4, '2024-02-10'), (9, 1, 4, NULL), (35, NULL, -1, '2024-02-05'), (28, NULL, NULL, '2024-02-20'), (1, 4, 9, '2024-02-28'), (16, 3, 6, '2024-02-01'), (24, 2, 3, NULL), (34, 6, -1, '2024-01-09'), (39, 4, -2, '2024-02-10'), (31, 6, 0, '2024-01-18'), (2, 1, -2, '2024-01-28'), (13, 4, 2, '2024-02-26'), (57, 4, 0, '2024-02-18'), (4, 1, 9, '2024-02-13'), (58, NULL, 8, '2024-02-11'), (7, NULL, -1, NULL), (27, 4, 9, '2024-01-15'), (14, 4, 4, NULL), (21, 2, 6, '2024-03-19'), (30, NULL, NULL, '2024-01-03');

CREATE INDEX t1_i580 ON t1 USING hash (c);

CREATE INDEX t1_i109 ON t1 USING hash (c);

CREATE INDEX t1_i547 ON t1 USING hash (id);

CREATE TABLE t2 (id int4 PRIMARY KEY, a int4, b int8, c text[], d numeric(10,2));

INSERT INTO t2 (id, a, b, c, d) VALUES (4, 7, 13, '{a,b}', NULL), (12, 9, 16, NULL, -2.00), (31, 0, 8, '{ab}', 12.25), (27, 6, 3, '{a,b}', -2.00), (28, 8, 7, '{a,b}', 8.00);

CREATE INDEX t2_i308 ON t2 (c);

CREATE INDEX t2_i379 ON t2 (b);

CREATE TABLE t3 (id int4, a int4, b interval, c text COLLATE "en-x-icu") PARTITION BY LIST (a);

CREATE TABLE t3_p0 PARTITION OF t3 FOR VALUES IN (0, 1, 2);

CREATE TABLE t3_p1 PARTITION OF t3 FOR VALUES IN (3, 4, NULL);

CREATE TABLE t3_pd PARTITION OF t3 DEFAULT;

INSERT INTO t3 (id, a, b, c) VALUES (26, -2, '2 hours', 'b');

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN false THEN RETURN sign((p & p))::int;
  ELSE RETURN p;
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE sql STABLE AS $$ SELECT id, c FROM t3 WHERE id <= lim $$;

CREATE VIEW v0 AS (SELECT true AS c0, mod('Infinity'::numeric, 3) AS c1, concat_ws('-', 'B', quote_literal(substr(t2.c, 2, 3))) AS c2 FROM t3 t1 JOIN t3 t2 USING (b)) UNION ALL (SELECT (('{}'::int4[] || t3.id) || (t3.b / 2)) IS DISTINCT FROM array_remove('{2,NULL}'::int4[], sign(0)::int) AS c0, (extract(day from '2000-05-20'::date)::int)::numeric AS c1, '9' AS c2 FROM t1 t3 WHERE true);

CREATE VIEW v1 AS SELECT mod(ceil(10.00::numeric), 3) AS c0, '{b,NULL}'::text[] AS c1 FROM ((SELECT 12::int2 AS c0, '{"a": 1, "b": "x"}'::json AS c1, concat_ws('-', t2.c, concat(left(t2.c, 0), CASE t2.id WHEN 5 THEN t2.a WHEN 0 THEN t2.a END)) AS c2 FROM t3 t2) s1 JOIN (SELECT min(NULL::timestamp) FILTER (WHERE (false OR t5.c0)) AS c0, array_agg(DISTINCT sign(sign(6)::int)::int ORDER BY sign(sign(6)::int)::int NULLS FIRST) AS c1 FROM (v0 t4 LEFT JOIN v0 t5 ON NULL::jsonb IS NOT DISTINCT FROM jsonb_strip_nulls('"s"'::jsonb)) GROUP BY t4.c2, t4.c1) s3 ON starts_with(concat_ws('-', s1.c2, s1.c2), s1.c2)) WHERE abs((0)::int8) > (-3)::int8;

MERGE INTO t0 x USING (SELECT id, b FROM t0 WHERE id % 3 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET a = (-2) WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (s.id + 100, DEFAULT, false);

INSERT INTO t0 AS x (id, a, b) VALUES (3, 9, false) ON CONFLICT (id) DO UPDATE SET a = ascii('z') WHERE x.b;

ANALYZE t0;

BEGIN;

DECLARE a45c1 CURSOR FOR SELECT position(concat_ws('-', replace(t1.c2, 'a', 'x'), upper(t1.c2)) in t1.c2) AS c0, ('{"a": 1, "b": "x"}'::json)::text::json AS c1 FROM v0 t1 WHERE t1.c0;

FETCH ALL FROM a45c1;

CLOSE a45c1;

COMMIT;

-- statement
SELECT upper('ab' COLLATE "und-x-icu") AS c0, min(0.5::float8) AS c1, '1999-11-17 11:59:00'::timestamp AS c2, ('{2,NULL}'::int4[])[1:2] AS c3 FROM (srf0((7 * 12)) s1 RIGHT JOIN (VALUES (false), (starts_with('a', s1.y))) s2(v0) ON s2.v0);
