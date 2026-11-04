-- class: eval_error
-- ordered: false
-- note: ok vs 22003: numeric field overflow
-- engine: ?column?:16 | d:1700
-- engine: -- INSERT 0 0
-- reference: ERROR 22003: numeric field overflow
-- reference: DETAIL: A field with precision 10, scale 2 must round to an absolute value less than 10^8.

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

DO $$ BEGIN RAISE NOTICE 'v=%', md5(('[]'::jsonb)::text); END $$;

UPDATE t3 x SET b = (x.b + ('2024-12-20 09:59:00'::timestamp - '2000-01-09 02:30:00'::timestamp)) WHERE ((SELECT (count(*))::int FROM t2 t1 WHERE CASE WHEN NULL::bool THEN CASE WHEN false THEN false ELSE true END ELSE true END), NULL::text) = (x.id, 'bar') RETURNING x.*;

DELETE FROM t2 x WHERE true RETURNING (-1.25::float8 + 3.75::float8) > ('Infinity'::float8 + NULL::float8);

BEGIN;

DECLARE a45c2 CURSOR FOR SELECT '2000-08-23 23:00:00-05'::timestamptz AS c0, (1::int8)::int4 AS c1, date_trunc('day', (s1.c0 AT TIME ZONE 'Asia/Kolkata')) AS c2 FROM (((SELECT ((('2000-07-27 00:59:00'::timestamp - '-1 mon'::interval) AT TIME ZONE 'UTC') AT TIME ZONE 'Asia/Kolkata') AS c0 FROM v0 t2 WHERE (t2.c2 IS JSON OBJECT)) s1 LEFT JOIN v0 t3 ON (SELECT (s4.o + 6::int8) FROM unnest('{5}'::int4[]) WITH ORDINALITY s4(u, o) WHERE cardinality('{3,1,2}'::int4[]) < CASE WHEN t3.c0 THEN s4.u ELSE s4.u END ORDER BY 1 LIMIT 1) NOT IN ((5::int8 + 2::int8), abs(1::int8), (6::int8 + 10::int8), abs(0::int8))) JOIN (SELECT (abs((SELECT cardinality('{2,NULL}'::int4[]) FROM ONLY t3 t9 ORDER BY 1 LIMIT 1)))::int2 AS c0, trim_scale((1::numeric / 3)) AS c1, 10::int8 AS c2 FROM (SELECT (-2147483648) AS c0, min(('2025-01-03'::date + t7.b)) AS c1, bit_or((cardinality('{}'::int4[]) - ((-4)::int8)::int4)) AS c2 FROM (t1 t7 JOIN t0 t8 ON t7.b = t8.a AND (3::int8 * NULL::int4) NOT IN ((t7.id)::int8, (5::int8 * t7.id), (t7.id)::int8)) WHERE (t8.b) IS NOT TRUE) s6 WHERE s6.c2 >= f0(s6.c2, initcap('z')) ORDER BY 1 DESC NULLS LAST, 2, 3 LIMIT 1) s5 ON t3.c1 = s5.c1 AND JSON_EXISTS(('{"a": {"b": 2}}'::jsonb || '{}'::jsonb), 'strict $.a.b')) WHERE isfinite(make_date(2024, 1, 1)) ORDER BY 1 NULLS FIRST, 2 DESC, 3 NULLS FIRST LIMIT 6;

FETCH ALL FROM a45c2;

CLOSE a45c2;

COMMIT;

ANALYZE t3;

WITH RECURSIVE r(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + 3) % 4, x || n::text FROM r WHERE length(x) < 4) SEARCH BREADTH FIRST BY n SET ord SELECT n, x, ord FROM r;

BEGIN;

DECLARE a45c4 CURSOR FOR SELECT min(((t2.id)::float8)::text) AS c0, upper(CASE WHEN false THEN 'Z' ELSE 'ba' END) AS c1, sum(mod(1.5::numeric, 3)) AS c2 FROM t3 t1 LEFT JOIN t1 t2 USING (id) WHERE isempty(int4range(LEAST(CASE WHEN 9::int8 BETWEEN 8::int8 AND 1::int8 THEN (1::int8)::int4 ELSE upper((int4range(LEAST(t2.id, (-4)), GREATEST(t2.id, (-4))) + int4range((-4), NULL))) END, t1.a), GREATEST(CASE WHEN 9::int8 BETWEEN 8::int8 AND 1::int8 THEN (1::int8)::int4 ELSE upper((int4range(LEAST(t2.id, (-4)), GREATEST(t2.id, (-4))) + int4range((-4), NULL))) END, t1.a))) GROUP BY t2.a ORDER BY 1 DESC NULLS LAST, 2 DESC NULLS LAST, 3 NULLS FIRST LIMIT 1;

FETCH ALL FROM a45c4;

CLOSE a45c4;

COMMIT;

DO $$ BEGIN RAISE NOTICE 'v=%', array_to_string(ARRAY['ß', 'a,b'], ',', '?'); END $$;

ANALYZE t3;

INSERT INTO t2 AS x (id, a, b, c, d) VALUES (56, -1, 7, NULL, 10.05) RETURNING x.*;

INSERT INTO t1 AS x (id, a, b, c) VALUES (2, 1, 3, '2024-01-21') RETURNING isempty((numrange(LEAST(round(-7.125::numeric, 3), mod(10.00::numeric, 3)), GREATEST(round(-7.125::numeric, 3), mod(10.00::numeric, 3))) * numrange(NULL, mod(10.00::numeric, 3)))), false;

-- statement
INSERT INTO t2 AS x (id, a, b, c, d) SELECT ((to_jsonb('[2:3]={7,8}'::int4[]) -> 'b')::jsonb ->> 'a')::int, s1.x, ((((-2) & s1.x) * f0(s1.x, s1.y)))::int8, LEAST(array_append('{ab}'::text[], (s1.y || s1.y)), array_append(array_append('{"x y",z}'::text[], s1.y), 'a')), (round((1e3::numeric * 1e20::numeric), 3) - (0::numeric)::int4) AS c4 FROM srf0((-2)) s1 WHERE '{}'::int4[] IS NOT NULL ON CONFLICT (id) DO UPDATE SET d = (('NaN'::numeric)::numeric(6,1))::numeric(6,1) WHERE (x.a, 'ab') < (EXCLUDED.id, 'aa') RETURNING true, x.d;
