-- class: error_detail
-- ordered: false
-- note: 42725
-- engine: ERROR 42725: function array_agg(unknown) is not unique
-- engine: POSITION: 133
-- reference: ERROR 42725: function array_agg(unknown) is not unique
-- reference: HINT: Could not choose a best candidate function. You might need to add explicit type casts.
-- reference: POSITION: 133

CREATE TABLE t0 (a date, b int8, c float8, d int4[]);

INSERT INTO t0 (a, b, c, d) VALUES ('2024-01-18', 3, -0.75, NULL), ('2024-03-20', 8, 4, NULL), ('2024-01-26', 5, -1.75, '{1,2}'), ('2024-03-04', NULL, 2.5, '{1,1,2}'), ('2024-01-03', 15, -0.75, '{NULL,3}');

CREATE TABLE t1 (id int4 PRIMARY KEY, a int8, b bool);

INSERT INTO t1 (id, a, b) VALUES (23, 19, true), (38, 12, false), (3, 15, true), (2, 8, true), (17, 16, true);

CREATE INDEX t1_i666 ON t1 ((id + 1));

CREATE INDEX t1_i595 ON t1 (a);

CREATE INDEX t1_i133 ON t1 (b);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN (3::int8 * p) > 8::int8 THEN p ELSE (- jsonb_array_length('{"a": 2.50}'::jsonb)) END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id FROM t1 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id FROM t1 WHERE id IS NULL;
  END IF;
END
$$;

INSERT INTO t1 AS x (id, a, b) VALUES (10, 6, true) ON CONFLICT DO NOTHING RETURNING (x.a NOT BETWEEN x.a AND x.a AND x.id < ANY ('{1,2}'::int4[])), 10.00::numeric;

WITH RECURSIVE r(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + 5) % 9, x || n::text FROM r WHERE length(x) < 4) SEARCH BREADTH FIRST BY n SET ord SELECT n, x, ord FROM r;

DO $$ BEGIN RAISE NOTICE 'v=%', ' x'; END $$;

SELECT count(*) OVER (ORDER BY 1 GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP) FROM (SELECT '2000-07-17 01:59:00'::timestamp AS c0, '{"a": 2.50}'::json AS c1, md5('a,b') AS c2, trunc((NULL::numeric * 1.5::numeric)) AS c3 FROM ((t0 t1 RIGHT JOIN t0 t2 ON t1.c = t2.c AND LEAST(t2.b, t1.b) NOT IN (SELECT abs(((t4.b + t3.b) * f0(4, 'B'))) AS c0 FROM (t0 t3 LEFT JOIN t0 t4 ON t3.a = t4.a AND NULL::bool) WHERE 'x ' >= left(trim('aa'), 1) COLLATE "C")) FULL JOIN ONLY t0 t5 ON t2.c = t5.c) WHERE false HAVING (1::numeric / 7) IN (avg(99.99::numeric))) s;

WITH RECURSIVE r(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + 1) % 9, x || n::text FROM r WHERE length(x) < 4) CYCLE n SET is_cycle USING path SELECT n, x, is_cycle, path FROM r;

DO $$ BEGIN RAISE NOTICE 'v=%', repeat(('{5}'::int4[])::text, 2); END $$;

INSERT INTO t0 AS x (a, b, c, d) SELECT max((t2.a - strpos('a%', 'a'))), (4::int8 * count('é') FILTER (WHERE true)), (- GREATEST(t1.c, t1.c)) AS c2, array_agg(LEAST(NULL::int4, 11) ORDER BY LEAST(NULL::int4, 11) NULLS FIRST) FROM (t0 t1 RIGHT JOIN t0 t2 ON t1.a = t2.a) GROUP BY t1.c, t2.a RETURNING (extract(day from x.a)::int / NULL::int4);

-- statement
SELECT count(*) OVER (ORDER BY 1 ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM (SELECT array_to_string(array_agg('foo' ORDER BY 'foo'), ',', '?') AS c0 FROM ((t1 t1 LEFT JOIN t0 t2 ON t1.a = t2.b AND t1.b) RIGHT JOIN t0 t3 ON t2.b = t3.b AND t2.c <> t3.c) WHERE t1.b) s;
