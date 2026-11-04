-- class: eval_error
-- ordered: false
-- note: 22008: cannot subtract infinite dates vs 22P02: invalid input syntax for type integer: "2.50"
-- engine: ERROR 22008: cannot subtract infinite dates
-- reference: ERROR 22P02: invalid input syntax for type integer: "2.50"

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

-- statement
WITH w1 AS MATERIALIZED (SELECT array_append(NULL::text[], regexp_replace((SELECT replace(s2.c1, 'a', 'x') FROM t0 t4 WHERE concat_ws('-', s2.c1, s2.c1) >= (s2.c3 ->> 'b') COLLATE "default" ORDER BY 1 LIMIT 1), '[ab]', 'y', 'g')) AS c0, (s2.c1 COLLATE "POSIX") AS c1, s2.c2 AS c2 FROM (SELECT DISTINCT mod('NaN'::numeric, 3) AS c0, regexp_replace(t3.d, '[ab]', 'y', 'g') AS c1, abs(((3::numeric)::numeric(6,1) - t3.id)) AS c2, jsonb_build_object('k', length(CASE WHEN t3.e THEN t3.d ELSE t3.d END), 'v', format('%s|%s', format('%s|%s', t3.d, t3.id), ascii(t3.d))) AS c3 FROM t3 t3 WHERE t3.e) s2), w5 AS (SELECT array_append('{ab}'::text[], concat('é', cardinality(s7.c0))) AS c0, LEAST(string_to_array(quote_literal(s7.c1), ','), '{ab}'::text[]) AS c1, '2 hours'::interval AS c2, ((1e3::numeric * (s6.u)::numeric) - (- (s6.u % 1))) AS c3 FROM (unnest('{1,1,2}'::int4[]) WITH ORDINALITY s6(u, o) FULL JOIN (SELECT NULL::int4[] AS c0, max(('a' COLLATE "und-x-icu")) AS c1 FROM ONLY t1 t8 WHERE false) s7 ON true) WHERE true) SELECT array_agg(DISTINCT (CASE WHEN true THEN '{"a": 2.50}'::jsonb ELSE '{"b": [1, "a"]}'::jsonb END::jsonb ->> 'a')::int ORDER BY (CASE WHEN true THEN '{"a": 2.50}'::jsonb ELSE '{"b": [1, "a"]}'::jsonb END::jsonb ->> 'a')::int NULLS FIRST) AS c0, ((2 - 12)) AS c1, t10.c0 AS c2 FROM ((t2 t9 RIGHT JOIN v0 t10 ON NOT (t10.c0 <= t10.c0)) JOIN LATERAL (SELECT array_agg(t12.id ORDER BY t12.id NULLS FIRST) FILTER (WHERE false) AS c0, (NULLIF(t12.b, t12.b) - max('2025-03-17 19:30:00'::timestamp)) AS c1, CASE WHEN NOT (t12.e) THEN '{a,b}'::text[] ELSE '{ab}'::text[] END AS c2 FROM t3 t12 GROUP BY GROUPING SETS ((t12.b), (t12.e)) HAVING ('{3,1,2}'::int4[] || '{1,2}'::int4[]) @> array_remove(NULL::int4[], (-1))) s11 ON t9.a = s11.c1 AND t9.a >= s11.c1) WHERE extract(day from '2025-05-11'::date)::int > (('2025-09-09'::date + 0) - 'infinity'::date) GROUP BY CUBE (t9.a, t10.c0, ((2 - 12))) HAVING true;
