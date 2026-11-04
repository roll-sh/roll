-- class: eval_error
-- ordered: false
-- note: ok vs 22000: range lower bound must be less than or equal to range upper bound
-- engine: c0:23 | c1:1184 | c2:23
-- engine: -- SELECT 0
-- reference: ERROR 22000: range lower bound must be less than or equal to range upper bound

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

SELECT count(*) OVER (ORDER BY 1 ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM (SELECT array_to_string(array_agg('foo' ORDER BY 'foo'), ',', '?') AS c0 FROM ((t1 t1 LEFT JOIN t0 t2 ON t1.a = t2.b AND t1.b) RIGHT JOIN t0 t3 ON t2.b = t3.b AND t2.c <> t3.c) WHERE t1.b) s;

UPDATE t0 x SET b = DEFAULT, c = DEFAULT WHERE x.d <@ (x.d || x.d) RETURNING 0::numeric;

DELETE FROM t1 x USING t0 u WHERE x.a = u.b AND NULLIF(x.id, x.id) <> ALL (ARRAY[x.id, x.id]) RETURNING regexp_replace(('{"a": null}'::jsonb)::text, '[ab]', 'y', 'g'), ((100::numeric)::int4 * ('null'::jsonb::jsonb ->> 'a')::int);

INSERT INTO t1 AS x (id, a, b) VALUES (49, -2, DEFAULT), (6, NULL, DEFAULT);

UPDATE t1 x SET a = s.v FROM (SELECT DISTINCT ON (id) id, a AS v FROM t0 ORDER BY id, 2) s WHERE s.id = x.id RETURNING x.*;

DELETE FROM t1 x WHERE x.b RETURNING x.*;

DO $$ BEGIN RAISE NOTICE 'v=%', quote_literal('A'); END $$;

MERGE INTO t1 x USING (SELECT id, b FROM t1 WHERE id % 3 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET a = x.a WHEN MATCHED AND (s.b AND s.b) THEN DELETE WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (s.id + 100, DEFAULT, false);

INSERT INTO t1 AS x (id, a, b) SELECT bit_or(2147483647), sum((('{"a": 1, "b": "x"}'::jsonb -> 'b')::jsonb ->> 'a')::int), bool_and(true) FROM (t0 t1 LEFT JOIN LATERAL (SELECT array_agg(3 ORDER BY 3 NULLS FIRST) FILTER (WHERE (true) IS NOT FALSE) AS c0, max(replace(reverse('B'), 'a', 'x')) AS c1 FROM t0 t3 GROUP BY t3.a, t3.b) s2 ON s2.c1 IN (SELECT DISTINCT s2.c1 AS c0 FROM (t0 t4 LEFT JOIN t0 t5 ON t4.a = t5.a) WHERE 'b' NOT IN (regexp_replace('9', '[ab]', 'y', 'g'), s2.c1, array_to_string('{A,a}'::text[], ',', '?')))) WHERE COALESCE(NOT EXISTS (SELECT COALESCE(CASE 2 WHEN 5 THEN t6.id WHEN 1 THEN t6.id END, t6.id) FROM (t1 t6 JOIN t0 t7 ON t6.a = t7.b) GROUP BY t6.id), ('NaN'::numeric / 7) = ALL (SELECT (10)::numeric AS c0 FROM (t1 t8 JOIN ONLY t0 t9 ON t8.a = t9.b) WHERE lpad((s2.c1 || s2.c1), 3, '*') >= s2.c1)) GROUP BY t1.c, s2.c0;

-- statement
WITH RECURSIVE r1(n, x) AS (SELECT 1, 'x'::text UNION ALL SELECT n + 1, x || 'y' FROM r1 WHERE n < 5), w2 AS NOT MATERIALIZED (SELECT date_trunc('day', '2024-02-13 03:00:00+00'::timestamptz) AS c0, ((int4range(LEAST(CASE WHEN false THEN CASE t4.n WHEN 1 THEN t4.n WHEN 4 THEN 0 END ELSE jsonb_array_length('[]'::jsonb) END, array_length((t3.d)[1:2], 1)), GREATEST(CASE WHEN false THEN CASE t4.n WHEN 1 THEN t4.n WHEN 4 THEN 0 END ELSE jsonb_array_length('[]'::jsonb) END, array_length((t3.d)[1:2], 1))) * int4range(NULL, array_length((t3.d)[1:2], 1))) -|- int4range(((t3.a + (-1)) - ('1999-07-22 23:00:00'::timestamp)::date), f0((t3.b)::int4, t4.x), '(]')) AS c1, '{}'::text[] AS c2 FROM (t0 t3 JOIN r1 t4 ON false)), w5 AS (SELECT NULL::interval AS c0, abs(3::numeric) AS c1, '{"a": {"b": 2}}'::json AS c2, ((SELECT array_agg(f0(2, 'ß') ORDER BY f0(2, 'ß') NULLS FIRST) FROM r1 t7 WHERE t6.c1) || NULL::int4[]) AS c3 FROM w2 t6 WHERE false) SELECT 0 AS c0, '2000-04-27 09:00:00+00'::timestamptz AS c1, lower(int4range(width_bucket(t8.c1, 0, 10, 4), ('2024-08-15'::date - '2025-12-18'::date), '()')) AS c2 FROM w5 t8 , r1 t9 FOR SHARE;
