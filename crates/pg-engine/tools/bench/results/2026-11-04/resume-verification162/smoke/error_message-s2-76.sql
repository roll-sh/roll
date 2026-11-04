-- class: error_message
-- ordered: false
-- note: column "t2.b" must appear in the GROUP BY clause or be used in an aggregate function | column "t2.id" must appear in the GROUP BY clause or be used in an aggregate function
-- engine: ERROR 42803: column "t2.b" must appear in the GROUP BY clause or be used in an aggregate function
-- engine: POSITION: 146
-- reference: ERROR 42803: column "t2.id" must appear in the GROUP BY clause or be used in an aggregate function
-- reference: POSITION: 268

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b text);

INSERT INTO t0 (id, a, b) VALUES (2, NULL, 'b');

CREATE INDEX t0_i991 ON t0 ((a + 1));

CREATE INDEX t0_i662 ON t0 USING hash (a);

CREATE TABLE t1 (a numeric(10,2), b float8 DEFAULT 1.75);

INSERT INTO t1 (a, b) VALUES (5.75, NULL), (-1.00, 2), (11.00, 0.25), (7.25, 0.5), (1.00, 2.25);

CREATE INDEX t1_i686 ON t1 USING hash (a);

CREATE INDEX t1_i845 ON t1 (a);

CREATE INDEX t1_i265 ON t1 (a) WHERE a IS NOT NULL;

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN true THEN abs((p * 7)) ELSE (SELECT bit_or((- 2)) FILTER (WHERE NOT (true)) FROM ONLY t1 t1 WHERE (q >= q COLLATE "C") IS TRUE) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql  AS $$ SELECT CASE WHEN sign(p)::int IN (SELECT min((f0((-2), q) % 1)) FROM ((SELECT LEAST('1999-10-03'::date, '2000-11-22'::date) AS c0 FROM t1 t2 WHERE (q SIMILAR TO '%(a|b)%')) EXCEPT ALL (SELECT ('2025-07-19 08:00:00'::timestamp)::date AS c0 FROM t0 t3 GROUP BY t3.b, (jsonb_array_length('{"a": 1, "b": "x"}'::jsonb)))) s1 , t1 t4 WHERE true GROUP BY t4.a, s1.c0) THEN p ELSE ((9223372036854775807::int8 * p))::int4 END $$;

UPDATE t1 x SET a = mod((0)::numeric, 3) WHERE EXISTS (SELECT 3 FROM (t0 t1 JOIN t1 t2 ON (SELECT bool_and(true) FROM t1 t3 WHERE false)) WHERE false) RETURNING extract(day from '2025-03-11'::date)::int;

DELETE FROM t1 x WHERE false RETURNING lower(NULL::text);

DELETE FROM t1 x WHERE to_char(3::numeric, 'FM999.00') = SOME (WITH w1 AS ((SELECT sum(DISTINCT t3.a) AS c0, 8 AS c1, max('2024-11-06'::date) AS c2 FROM (t1 t2 JOIN t1 t3 ON mod(t2.a, 3) >= t3.a) GROUP BY t3.a) EXCEPT ALL (SELECT mod(x.a, 3) AS c0, (ARRAY[array_length('{1,2}'::int4[], 1), position(t4.b in t4.b)])[1] AS c1, '1999-04-15'::date AS c2 FROM t0 t4)), w5 AS NOT MATERIALIZED (WITH w6 AS (SELECT max(upper('10')) AS c0 FROM (t1 t7 JOIN w1 t8 ON t7.a = t8.c0 AND CASE WHEN true THEN false ELSE '{b,NULL}'::text[] IS DISTINCT FROM '{"x y",z}'::text[] END) WHERE false GROUP BY t7.b), w9 AS (SELECT min(CASE WHEN false THEN (NULL::date - t10.c1) ELSE ('infinity'::date - 11) END) AS c0 FROM w1 t10 GROUP BY t10.c0) SELECT true AS c0 FROM (t0 t11 JOIN t1 t12 ON false) WHERE NULL::bool) SELECT string_agg('Ab', ',' ORDER BY 'Ab') AS c0 FROM ((VALUES (NULL, NULL, 3::numeric)) s13(v0, v1, v2) JOIN LATERAL (WITH RECURSIVE r15(n, x) AS (VALUES (0, ''::text) UNION SELECT (n * 2 + 1) % 7, x || n::text FROM r15 WHERE length(x) < 7), w16 AS MATERIALIZED (SELECT s13.v2 AS c0, concat_ws('-', reverse(t17.b), lower('a%')) < split_part(t17.b, ',', 1) COLLATE "und-x-icu" AS c1, ('{5}'::int4[] || (CASE WHEN true THEN t17.a ELSE t17.a END)::text::int4) AS c2 FROM t0 t17 WHERE true) SELECT (date_part('month', NULLIF((s13.v1)::timestamp, ('1999-09-15 16:59:00'::timestamp)::text::timestamp)))::int AS c0 FROM (r15 t18 LEFT JOIN w5 t19 ON to_char(NULL::numeric, 'FM999.00') < to_char(x.a, 'FM999.00'))) s14 ON NULL::bool) WHERE abs(COALESCE(5::int8, 6::int8)) IS NOT NULL GROUP BY s13.v2 HAVING format('%s|%s', 'ab', NULL::int4) <> SOME (SELECT 'ba' FROM (w1 t20 FULL JOIN w5 t21 ON t21.c0) WHERE t21.c0));

UPDATE t0 x SET b = 'foo', a = 4 WHERE x.b LIKE '[0-9]' RETURNING COALESCE(NULLIF(0.001::numeric, 10.00::numeric), mod(-7.125::numeric, 3)), 0::numeric;

DO $$ BEGIN RAISE NOTICE 'v=%', trim(('2000-11-03'::date)::text); END $$;

DO $$ BEGIN RAISE NOTICE 'v=%', quote_literal(to_char(0::numeric, 'FM999.00')); END $$;

INSERT INTO t1 AS x (a, b) (SELECT (t1.a)::numeric AS c0, -0::float8 AS c1 FROM t0 t1 WHERE (3.75::float8 * abs(-1.25::float8)) > ((2::float8 + 0.5::float8) * 'Infinity'::float8)) EXCEPT (SELECT round(('NaN'::numeric / 2.5), 1) AS c0, ((t2.a)::float8 + LEAST((t2.a)::float8, 0.5::float8)) AS c1 FROM t0 t2 WHERE (((9223372036854775807::int8)::int4 * position(t2.b in 'ab')), upper(('1'::jsonb ->> 'b'))) <> (LEAST(position(t2.b in 'z'), NULL::int4), t2.b)) RETURNING '10';

UPDATE t1 x SET b = (length('ba'))::float8 WHERE (((-2147483648) - (-4)) IN (SELECT t2.id FROM (t1 t1 LEFT JOIN t0 t2 ON t1.b < t1.b) WHERE starts_with(quote_literal('ß'), (('{"a": 1, "b": "x"}'::jsonb - 'a') ->> 'a'))) AND NOT (NULL::bool)) RETURNING x.*;

INSERT INTO t0 AS x (id, a, b) SELECT count(*) AS c0, ((11::int8)::int4 + width_bucket(3::numeric, 0, 10, 4)) AS c1, string_agg(left(concat(t2.b, t1.id), -1), ',' ORDER BY left(concat(t2.b, t1.id), -1)) FROM (t0 t1 RIGHT JOIN t0 t2 ON t1.id = t2.id) GROUP BY ROLLUP (t1.id, t2.id) ON CONFLICT (id) DO UPDATE SET a = ascii(EXCLUDED.b) WHERE 'B' SIMILAR TO '^a' RETURNING x.*;

ANALYZE t1;

ANALYZE t1;

INSERT INTO t0 AS x (id, a, b) SELECT DISTINCT abs(width_bucket(abs(t2.a), 0, 10, 4)), 1, substr(CASE WHEN (false) IS TRUE THEN 'a_b' ELSE trim('A') END, 3, 3) FROM t1 t2 WHERE true RETURNING x.*;

-- statement
SELECT 0.5::float8 AS c0, '{"a": 2.50}'::json AS c1 FROM (SELECT CASE WHEN true THEN 9::int2 ELSE 5::int2 END AS c0, (SELECT string_agg(GREATEST(t2.b, t2.b), ',' ORDER BY GREATEST(t2.b, t2.b)) FROM t1 t4) AS c1, NULL::numeric AS c2, ARRAY[(CASE WHEN true THEN 8 ELSE t2.id END & jsonb_array_length('[]'::jsonb)), length(split_part(t2.b, ',', 1))] AS c3 FROM t0 t2 WHERE (SELECT every(t3.b > 'NaN'::float8) FROM t1 t3 WHERE (t2.b IN ('Z', t2.b, t2.b) OR '{a,b}'::text[] <= '{a,b}'::text[]))) s1 WHERE NOT EXISTS ((SELECT position(lower(quote_literal(t5.b)) in t6.b) AS c0 FROM (t0 t5 FULL JOIN t0 t6 ON t5.id = t6.id AND t5.id < t6.id) WHERE NULLIF(false, true)) INTERSECT ALL (SELECT ((- t7.id)) AS c0 FROM t0 t7 WHERE isfinite(('1999-01-12'::date + t7.id)) GROUP BY GROUPING SETS ((t7.b), (((- t7.id))))));
