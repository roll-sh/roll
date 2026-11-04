-- class: error_message
-- ordered: true
-- note: cannot delete from scalar | cannot get array length of a scalar
-- engine: ERROR 22023: cannot delete from scalar
-- reference: ERROR 22023: cannot get array length of a scalar

CREATE TABLE t0 (id int4, a json, b int8 DEFAULT 10);

INSERT INTO t0 (id, a, b) VALUES (1, '[1, 2, 3]', -2), (13, '{"b": [1, "a"]}', 9), (19, '[]', 19), (14, '{"a": 3}', 18), (18, '{"a": "1"}', 13);

CREATE TABLE t1 (id int4 PRIMARY KEY, a text, b int4[], c numeric(10,2));

INSERT INTO t1 (id, a, b, c) VALUES (36, 'foo', '{1,2}', 8.75), (34, NULL, '{2}', -2.00), (8, '9', '{}', 3.50), (10, 'b', '{1,2}', 3.75), (40, 'bar', '{NULL,3}', 3.25), (11, 'a,b', '{1,2}', -1.05), (22, '10', '{NULL,3}', 8.00), (13, 'foo', '{5}', NULL), (35, 'b', '{3,1,2}', NULL), (23, '9', '{5}', 5.50), (2, '9', '{5}', -1.50), (6, ' x', '{}', 12.05), (26, ' x', '{3,1,2}', -1.75), (57, 'Ä', '{3,1,2}', 7.05), (43, '10', '{NULL,3}', 4.75), (48, 'foo', '{5}', 5.25), (15, 'abc', '{1,2}', -3.05), (31, ' x', '{NULL,3}', 3.00), (37, 'x ', NULL, 3.75), (18, 'a,b', '{5}', 7.00), (9, 'x ', '{5}', 8.25), (5, 'a', '{3,1,2}', 4.00), (55, ' x', '{2}', 7.00), (49, 'a_b', '{2}', 2.75), (3, 'foo', '{1,1,2}', NULL), (45, NULL, '{NULL,3}', NULL), (32, NULL, '{NULL,3}', 1.00), (41, 'B', '{NULL,3}', -1.50), (16, NULL, '{5}', NULL), (58, 'a,b', '{1,2}', 1.50);

CREATE INDEX t1_i442 ON t1 (c);

CREATE TABLE t2 (id int4 PRIMARY KEY, a bool, b int4, c int4, d jsonb);

INSERT INTO t2 (id, a, b, c, d) VALUES (21, true, NULL, NULL, '{"a": {"b": 2}}'), (4, true, -1, 8, '[]'), (33, false, 3, 4, '[]'), (6, NULL, 6, 9, '1'), (9, true, 2, 5, 'null'), (43, true, 3, 6, '1'), (13, false, 5, NULL, '{"a": "1"}'), (2, true, 2, -1, 'null');

CREATE INDEX t2_i80 ON t2 (c);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int8, b json, c int8, d text DEFAULT 'a_b', e float8);

INSERT INTO t3 (id, a, b, c, d, e) VALUES (28, 1, '{"a": 3}', 2, 'A', -1.25), (16, -2, '{"a": 3}', 18, '', -1.75), (19, 8, 'null', NULL, 'ab', -1.5);

CREATE INDEX t3_i226 ON t3 (e, d);

ANALYZE;

CREATE VIEW v0 AS SELECT DISTINCT jsonb_array_length(jsonb_build_object('k', extract(day from '2024-09-20'::date)::int, 'v', (s2.c3 || s2.c3))) AS c0 FROM (t2 t1 RIGHT JOIN (WITH w3 AS NOT MATERIALIZED (SELECT t5.id AS c0, '2024-06-02 09:00:00+02:30'::timestamptz AS c1, (((1e3::numeric / 7) + (0.001::numeric + 1::numeric)) / 7) AS c2 FROM t0 t4 , t3 t5 WHERE NULLIF(NULLIF(t5.a, t5.c) <= (t5.id)::int8, jsonb_strip_nulls('1'::jsonb) ?| ARRAY['a', 'x'])) SELECT (t6.c + trunc(mod(t6.c, 3))) AS c0, '2000-12-10 15:00:00+02:30'::timestamptz AS c1, (((3::numeric + t6.c) * mod(100::numeric, 3)) * ((t6.c + -7.125::numeric) / 3)) AS c2, 'x ' AS c3 FROM t1 t6 WHERE (jsonb_build_array(jsonb_build_object('k', 12, 'v', 'ß'), 'NaN'::numeric) -> 1) ? 'a') s2 ON t1.c NOT IN (SELECT NULLIF(array_length('{}'::int4[], 1), count(*)) AS c0 FROM t3 t7 WHERE jsonb_strip_nulls(COALESCE('[]'::jsonb, t1.d)) ?| ARRAY['a', 'x'] GROUP BY t7.d)) WHERE (int4range(t1.b, t1.id, '[)') @> (('{1,2}'::int4[] || '{1,2}'::int4[]))[1]);

DO $$ BEGIN RAISE NOTICE 'v=%', ('0'::interval)::text; END $$;

UPDATE t0 x SET b = (abs(x.b) * extract(day from '2024-10-17'::date)::int) WHERE true RETURNING ((NULL::numeric / 7) * (12345678.9::numeric)::numeric(6,1));

INSERT INTO t0 AS x (id, a, b) SELECT ('2025-06-28'::date - ('1999-01-18'::date + length('A'))), (json_build_array(t1.c0, 'a%') -> 'a'), abs(abs(NULL::int8)) FROM v0 t1;

UPDATE t0 x SET b = x.b, a = ((x.a -> 'a') -> 'a') WHERE '1 day'::interval IS NOT NULL RETURNING cardinality('{}'::int4[]);

UPDATE t2 x SET a = false FROM (SELECT DISTINCT ON (id) id, a AS v FROM t1 ORDER BY id, 2) s WHERE s.id = x.id;

INSERT INTO t0 AS x (id, a, b) SELECT position(CASE WHEN (NULL::bool AND true) THEN lower(s2.v2) ELSE s2.v2 END in s2.v2), t1.a AS c1, abs(t1.b) FROM (t0 t1 JOIN (VALUES (12345678.9::numeric, 1e3::numeric, CASE WHEN false THEN 'Ä' ELSE 'foo' END), (12345678.9::numeric, -0.25::numeric, '10'), (1e3::numeric, 10.00::numeric, 'ba')) s2(v0, v1, v2) ON false);

MERGE INTO t1 x USING (SELECT id FROM t2 WHERE id % 3 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET b = array_remove(x.b, (8 * x.id)) WHEN NOT MATCHED BY SOURCE THEN DELETE;

UPDATE t3 x SET a = (sign(x.id)::int)::int8 WHERE (quote_literal('foo') COLLATE "und-x-icu") > x.d;

INSERT INTO t1 AS x (id, a, b, c) (SELECT t1.c0 AS c0, string_agg('ß', ',' ORDER BY 'ß') FILTER (WHERE true) AS c1, array_agg(((2.50::numeric - t1.c0))::int4 ORDER BY ((2.50::numeric - t1.c0))::int4 NULLS FIRST) AS c2, ceil(max(0.001::numeric)) AS c3 FROM v0 t1 WHERE true GROUP BY ROLLUP (t1.c0, (length('bar')))) UNION (SELECT t3.b AS c0, regexp_replace(CASE WHEN t3.id < 4 THEN repeat('Z', 2) ELSE NULL::text END, '[ab]', 'y', 'g') AS c1, ARRAY[((t2.id / 4) - COALESCE(11, t2.id)), (- t3.id)] AS c2, t2.c AS c3 FROM t1 t2 JOIN t2 t3 USING (id) WHERE jsonb_build_object('k', (10 & t3.b), 'v', to_char(t2.c, 'FM999.00')) = (jsonb_strip_nulls(t3.d) -> 'a')) ON CONFLICT DO NOTHING;

DO $$ BEGIN RAISE NOTICE 'v=%', (10.00::numeric)::text; END $$;

-- statement
SELECT (count(*))::int AS c0, sum(((s1.c0 + 2) - s1.c1)) FILTER (WHERE true) AS c1, max(jsonb_array_length('null'::jsonb)) AS c2, (count(*))::int AS c3 FROM ((SELECT (((NULL::date)::timestamp)::date - (('{"a": null}'::jsonb - 'a')::jsonb ->> 'a')::int) AS c0, (NULLIF('2000-09-03'::date, ('2000-02-10'::date - t2.id)) - extract(day from (SELECT max(('2000-06-07'::date + t3.id)) FROM t3 t3 WHERE false))::int) AS c1, NULL::text AS c2, array_append(string_to_array('a_b', ','), quote_literal('b')) AS c3 FROM t0 t2 WHERE (('"s"'::jsonb - 'a') -> 'a') @> jsonb_build_object('k', NULLIF(t2.id, t2.id), 'v', '10')) s1 LEFT JOIN unnest('{1,2}'::int4[]) WITH ORDINALITY s4(u, o) ON (SELECT bool_and(s5.c1 = ANY (SELECT count(*) AS c0 FROM t2 t8 CROSS JOIN v0 t9 WHERE NULLIF(s1.c3 @> '{ab}'::text[], t8.a) GROUP BY GROUPING SETS ((t9.c0, t8.d), ()))) FROM (SELECT DISTINCT (NULL::timestamp AT TIME ZONE 'UTC') AS c0, position((lower('abc') || (t7.c)::text) in 'foo') AS c1 FROM (t1 t6 JOIN t1 t7 ON NULL::bool) WHERE false) s5 WHERE '1 mon -1 day'::interval IS NULL)) WHERE EXISTS (SELECT bit_and((('1'::jsonb -> 'b')::jsonb ->> 'a')::int) FROM ((SELECT trim(s1.c2) AS c0 FROM t0 t11 CROSS JOIN v0 t12 WHERE COALESCE(LEAST(s1.c2, s1.c2), format('%s|%s', s1.c2, t11.id)) LIKE 'A%') s10 JOIN (VALUES ('Ä')) s13(v0) ON s10.c0 = s13.v0 AND GREATEST(-0.25::numeric, 1e3::numeric) <> SOME (SELECT ((- t14.id))::numeric AS c0 FROM t1 t14 WHERE NULL::bool)) WHERE (false) IS TRUE GROUP BY s13.v0) GROUP BY s4.o, s1.c1 ORDER BY 1 DESC NULLS LAST, 2, 3, 4 NULLS FIRST;
