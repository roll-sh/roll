-- class: eval_error
-- ordered: true
-- note: ok vs 22003: integer out of range
-- engine: c0:21 | c1:21 | c2:21 | c3:701
-- engine: -- SELECT 0
-- reference: ERROR 22003: integer out of range

CREATE TABLE t0 (id int4, a int4, b text[]) PARTITION BY HASH (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES WITH (MODULUS 3, REMAINDER 0);

CREATE TABLE t0_p1 PARTITION OF t0 FOR VALUES WITH (MODULUS 3, REMAINDER 1);

CREATE TABLE t0_p2 PARTITION OF t0 FOR VALUES WITH (MODULUS 3, REMAINDER 2);

INSERT INTO t0 (id, a, b) VALUES (37, 7, '{z}'), (14, -2, '{a,b}'), (27, 7, NULL), (11, 1, '{z}'), (3, 4, '{a,b}');

CREATE INDEX t0_i322 ON t0 (b);

CREATE TABLE t1 (id int4 PRIMARY KEY, a numeric(10,2), b numeric(10,2), c numeric(10,2), d interval, e json);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (29, 0.75, -3.00, 4.75, '24:00:00', '{"a": {"b": 2}}'), (27, -3.00, 10.75, 0.00, '2 hours', NULL), (15, 8.00, 12.25, 10.75, '0', '[]'), (40, NULL, -1.05, -3.50, '24:00:00', '{"b": [1, "a"]}'), (25, 12.50, 0.05, 9.05, '24:00:00', '{"a": 3}'), (38, 3.75, -3.25, 8.05, '30 days', '1'), (36, 12.75, NULL, 9.00, '1 day', NULL), (14, -2.25, 6.00, 6.50, '2 hours', 'null'), (12, 6.25, 4.25, -1.00, '0', '{"a": {"b": 2}}'), (32, 2.00, 7.75, 11.05, NULL, '[1, 2, 3]'), (39, 6.25, 11.00, 11.25, NULL, '{"a": "1"}'), (31, 9.50, 0.50, -3.25, NULL, NULL), (54, NULL, 5.50, 5.75, '-1 day', '{"a": 1, "b": "x"}'), (19, NULL, NULL, NULL, '1 mon', '{"b": [1, "a"]}'), (22, 5.00, 2.00, 5.50, '2 hours', '"s"'), (4, 3.00, 1.25, -1.25, '1 mon', '{"b": [1, "a"]}'), (8, 3.00, 5.25, 8.00, '-1 day', '[]'), (20, 5.75, 6.00, 3.50, '2 hours', '[1, 2, 3]'), (18, 10.50, -1.50, 10.05, '1 day', '[1, 2, 3]'), (13, NULL, -1.75, -2.25, NULL, '{"a": 3}'), (21, 3.00, NULL, 11.05, '0', NULL), (42, 4.00, -1.00, -2.75, '-1 day', '{"a": "1"}'), (48, -2.00, -1.50, NULL, '-1 day', '{"b": [1, "a"]}'), (45, 11.50, 5.25, NULL, '24:00:00', '[]'), (23, 12.00, 2.00, 8.75, '-1 day', '[1, 2, 3]'), (47, 0.00, 11.00, NULL, '0', '{"a": {"b": 2}}'), (7, 4.05, -2.00, 0.50, '30 days', '{"b": [1, "a"]}'), (56, NULL, NULL, NULL, '24:00:00', '[]'), (57, 7.50, 6.25, 3.50, '0', '{"a": "1"}'), (51, 2.00, NULL, 5.00, '0', NULL);

CREATE INDEX t1_i215 ON t1 (id);

CREATE TABLE t2 (a interval, b bool, c int8);

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2_c (a, b, c, extra) VALUES ('0', true, NULL, 1);

INSERT INTO t2_c (a, b, c, extra) VALUES ('2 hours', false, 5, 3);

INSERT INTO t2_c (a, b, c, extra) VALUES (NULL, true, NULL, 2);

CREATE INDEX t2_i588 ON t2 (c, b);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int2, b timestamp, c bool, d text COLLATE "en-x-icu", e bool DEFAULT true);

INSERT INTO t3 (id, a, b, c, d, e) VALUES (22, NULL, NULL, true, NULL, false), (17, 3, '2024-01-05 06:30:00', false, 'Z', true), (27, 3, '2024-01-05 12:30:00', false, 'abc', false);

CREATE INDEX t3_i134 ON t3 (e) WHERE e IS NOT NULL;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN sign(p)::int IN (SELECT CASE ascii(t4.d) WHEN 3 THEN ((SELECT LEAST(t5.id, t5.id) FROM t1 t5 WHERE (t5.id, initcap('9')) <= (t5.id, left(t4.d, 2)) ORDER BY 1 LIMIT 1) % 1) WHEN 1 THEN 6 END AS c0 FROM ((SELECT LEAST('1999-10-03'::date, '2000-11-22'::date) AS c0 FROM t1 t2 WHERE (q SIMILAR TO '%(a|b)%')) EXCEPT ALL (SELECT ('2025-07-19 08:00:00'::timestamp)::date AS c0 FROM t0 t3 GROUP BY t3.b, (jsonb_array_length('{"a": 1, "b": "x"}'::jsonb)))) s1 , t3 t4 WHERE true) THEN COALESCE(((-4))::text::int4, 10) ELSE jsonb_array_length(NULLIF('{"a": 1, "b": "x"}'::jsonb, 'null'::jsonb)) END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE sql STABLE AS $$ SELECT id, d FROM t3 WHERE id <= lim $$;

CREATE VIEW v0 AS WITH RECURSIVE r1(n, x) AS (VALUES (0, ''::text) UNION SELECT (n * 2 + 1) % 7, x || n::text FROM r1 WHERE length(x) < 6), w2 AS (WITH RECURSIVE r3(n, x) AS (VALUES (0, ''::text) UNION SELECT (n * 2 + 1) % 7, x || n::text FROM r3 WHERE length(x) < 6), w4 AS NOT MATERIALIZED (SELECT round(trunc(1::numeric), 0) AS c0, ('2 hours'::interval * 10) AS c1 FROM t2 t5 WHERE false GROUP BY GROUPING SETS ((t5.c), (t5.a))), w6 AS (SELECT DISTINCT (('{}'::int4[])[1:2])[1] AS c0, (regexp_instr(md5(lower('a_b')), 'b')) AS c1 FROM t0 t7) SELECT NULL::numeric AS c0, 'a' AS c1, -1::int2 AS c2, '2024-01-09 03:00:00+00'::timestamptz AS c3 FROM t2 t8 ORDER BY 1 DESC, 2, 3 NULLS FIRST, 4) SELECT 'x ' AS c0, sum(s9.v0) FILTER (WHERE NULL::text = ALL ('{a,b}'::text[])) AS c1 FROM ((VALUES (0::numeric, 8), (12345678.9::numeric, 10)) s9(v0, v1) JOIN ((SELECT avg(DISTINCT (-2)::int8) AS c0, sum((NULLIF((-1)::int8, 6::int8) * t12.id)) FILTER (WHERE t11.c) AS c1, (abs(-7.125::numeric) / 3) AS c2 FROM (t3 t11 LEFT JOIN t3 t12 ON '{2,NULL}'::int4[] <@ (NULL::int4[] || '{3,1,2}'::int4[])) WHERE NOT ('NaN'::float8 > (8)::float8) GROUP BY t12.d, t11.e) INTERSECT ALL (SELECT (t13.id)::numeric AS c0, mod(((-0.25::numeric / 3) / 3), 3) AS c1, COALESCE(t13.a, ceil(t13.b)) AS c2 FROM t1 t13 WHERE JSON_OBJECT('a': array_length('{}'::int4[], 1), 'b': (t13.id)::text  RETURNING jsonb) ? 'k')) s10 ON s9.v0 = s10.c0) WHERE false HAVING starts_with('é', NULLIF('Ä', 'ba'));

UPDATE t1 x SET c = ceil(abs(x.c)) WHERE COALESCE((NULLIF(false, false) AND x.id IS NOT DISTINCT FROM x.id), false) RETURNING x.*;

UPDATE t3 x SET e = array_append('{A,a}'::text[], 'ab') <= string_to_array(x.d, ',');

DO $$ BEGIN RAISE NOTICE 'v=%', 'b'; END $$;

UPDATE t2 x SET c = abs((2)::int8) WHERE EXISTS (SELECT s1.v0 FROM (VALUES (10, NULL, 'aa'), (NULL, 'ba', 'ab'), (6, '', 'bar')) s1(v0, v1, v2) WHERE (SELECT every(t2.e) FROM t3 t2)) RETURNING true;

UPDATE t1 x SET e = json_build_array(array_length('{NULL,3}'::int4[], 1), COALESCE('a%', '10')) FROM (SELECT DISTINCT ON (id) id, e AS v FROM t0 ORDER BY id, 2) s WHERE s.id = x.id;

UPDATE t3 x SET c = x.e WHERE '{}'::int4[] <> ('{NULL,3}'::int4[] || ARRAY[x.id, x.id]) RETURNING x.id;

UPDATE t2 x SET b = CASE WHEN (4, 'bar') <> ((-3), 'a') THEN x.b ELSE (SELECT bool_or(t1.e) FROM t3 t1 WHERE EXISTS (SELECT f0((x.c)::int4, t1.d) FROM t0 t2 WHERE starts_with(quote_literal(t1.d), to_char(COALESCE(3::numeric, 0::numeric), 'FM999.00')))) END, a = justify_hours((x.a + x.a)) RETURNING x.*;

UPDATE t2 x SET a = DEFAULT, b = ((-4))::numeric <= 'NaN'::numeric;

DO $$ BEGIN RAISE NOTICE 'v=%', 'x '; END $$;

INSERT INTO t0 AS x (id, a, b) SELECT DISTINCT (('"s"'::jsonb - 'a')::jsonb ->> 'a')::int, array_position((('{1,2}'::int4[] || (-4)) || 5), (- (1.5::numeric)::int4)), s1.c1 FROM (SELECT '{"a": {"b": 2}}'::json AS c0, ARRAY[NULL::text, md5(CASE WHEN false THEN 'foo' ELSE ' x' END)] AS c1 FROM (t0 t2 FULL JOIN t0 t3 ON t2.a = t3.a) WHERE (('{NULL,3}'::int4[])[1:2])[1:2] && ('{3,1,2}'::int4[])::text::int4[]) s1 RETURNING md5('a_b');

INSERT INTO t3 AS x (id, a, b, c, d, e) SELECT (count(*))::int, max(t1.a), '2024-03-03 19:30:00'::timestamp, bool_or(CASE WHEN '{1,1,2}'::int4[] <= '{3,1,2}'::int4[] THEN CASE WHEN t1.c THEN t1.e ELSE t1.c END ELSE t1.e END), string_agg(array_to_string(array_append('{b,NULL}'::text[], t1.d), ',', '?'), ',' ORDER BY array_to_string(array_append('{b,NULL}'::text[], t1.d), ',', '?') DESC) AS c4, isfinite(('2024-07-16 04:59:00'::timestamp)::date) FROM (t3 t1 JOIN (WITH RECURSIVE r3(n, x) AS (SELECT 1, 'x'::text UNION ALL SELECT n + 1, x || 'y' FROM r3 WHERE n < 6), w4 AS (SELECT 7 AS c0 FROM v0 t5), w6 AS MATERIALIZED (SELECT mod(ceil(round(-0.25::numeric, 1)), 3) AS c0, NULLIF(-1.25::float8, 3.75::float8) AS c1 FROM t3 t7 WHERE (COALESCE((5, t7.d) <= (4, t7.d), '{"a": null}'::jsonb ?| ARRAY['a', 'x'])) IS TRUE) SELECT (json_build_array(t8.id, t8.d) -> 'a') AS c0, t8.a AS c1 FROM t3 t8 , w6 t9) s2 ON 'z' !~ '%') GROUP BY t1.e HAVING ('1999-05-22 09:30:00'::timestamp - '-2 days'::interval) >= ('2000-01-15'::date)::timestamp ON CONFLICT DO NOTHING RETURNING -7.125::numeric;

ANALYZE t0;

INSERT INTO t1 AS x (id, a, b, c, d, e) VALUES (18, DEFAULT, 11.25, 3.50, '1 mon', '{"a": 1, "b": "x"}');

DO $$ BEGIN RAISE NOTICE 'v=%', repeat('Ä', 2); END $$;

INSERT INTO t3 AS x (id, a, b, c, d, e) SELECT (COALESCE(abs(t1.c), (4)::int8))::int4, CASE WHEN NOT EXISTS (SELECT 6 AS c0 FROM t3 t2 WHERE format('%s|%s', replace('B', 'a', 'x'), COALESCE(t2.id, t2.id)) NOT BETWEEN t2.d AND t2.d) THEN (3)::int2 ELSE -2::int2 END, ((make_date(2024, 3, 1))::timestamp - t1.a) AS c2, t1.b, (CASE WHEN '{"a": 2.50}'::jsonb @> NULL::jsonb THEN '10' ELSE md5('ba') END || array_to_string('{A,a}'::text[], ',', '?')) AS c4, t1.b FROM t2 t1 ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c RETURNING (11::int8)::int4, lpad(replace(x.d, 'a', 'x'), 3, '*');

INSERT INTO t2 AS x (a, b, c) SELECT '2 hours'::interval, NULLIF(s1.v2, (COALESCE(s1.v2, s1.v2)) IS NOT TRUE), (8::int8 + 1::int8) FROM (VALUES (2147483647, 0, false), (8, 5, false), (9, NULL, NULL), (2, NULL, true)) s1(v0, v1, v2) WHERE starts_with(to_char(10.00::numeric, 'FM999.00'), to_char(mod(99.99::numeric, 3), 'FM999.00')) RETURNING x.*;

UPDATE t2 x SET b = CASE WHEN x.b THEN 'ß' <> 'Ä' ELSE x.b END;

-- statement
SELECT ((2147483647 + GREATEST(2147483647, 7)))::int2 AS c0, (sign((6 + 12))::int)::int2 AS c1, ((position('a%' in '10'))::int2 + -3::int2) AS c2, (- -1.25::float8) AS c3 FROM (VALUES (1e3::numeric, true), (0.001::numeric, true), (10.00::numeric, true)) s1(v0, v1) WHERE repeat(lower('9'), 2) <= upper(NULLIF(NULL::text, 'a')) ORDER BY 1 DESC NULLS LAST, 2, 3 DESC, 4 DESC NULLS LAST LIMIT 5;
