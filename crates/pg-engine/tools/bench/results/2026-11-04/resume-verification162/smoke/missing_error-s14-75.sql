-- class: missing_error
-- ordered: true
-- note: 0A000: FOR KEY SHARE cannot be applied to the nullable side of an outer join
-- engine: c0:1007 | c1:25 | c2:1009 | c3:25
-- engine: -- SELECT 0
-- reference: ERROR 0A000: FOR KEY SHARE cannot be applied to the nullable side of an outer join

CREATE TABLE t0 (id int4, a numeric(10,2) DEFAULT 4.50, b int8);

INSERT INTO t0 (id, a, b) VALUES (11, 7.05, 11), (3, 2.50, 19), (14, 6.25, 16);

CREATE INDEX t0_i810 ON t0 (a);

CREATE INDEX t0_i987 ON t0 (id, a);

CREATE INDEX t0_i351 ON t0 (b) WHERE b IS NOT NULL;

CREATE TABLE t1 (a bool, b int4 DEFAULT 3, c interval, d float8, e text DEFAULT 'B');

INSERT INTO t1 (a, b, c, d, e) VALUES (true, 5, '-1 day', 2.75, 'z'), (false, 6, '30 days', NULL, NULL), (NULL, 0, NULL, -0.5, 'z'), (true, 2, '24:00:00', NULL, '10'), (true, 1, '30 days', 1.5, 'z'), (false, 1, '30 days', 1, 'Ä'), (true, NULL, '30 days', NULL, ' x'), (true, 2, NULL, 0.5, 'z'), (false, NULL, '0', 3.75, 'a,b'), (true, 9, '-1 day', -0.75, ' x'), (true, -2, NULL, -0.25, 'A'), (false, 3, '0', -1, 'a,b');

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT b, e FROM t1 WHERE b <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT b, e FROM t1 WHERE b IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS (SELECT (('1999-08-05'::date)::text::date + (t1.id - t1.id)) AS c0, percentile_disc(1) WITHIN GROUP (ORDER BY (('{2,NULL}'::int4[])[1])::numeric) AS c1, bool_or(CASE WHEN true THEN '{"a": {"b": 2}}'::jsonb ? 'b' ELSE true END) AS c2 FROM t0 t1 GROUP BY GROUPING SETS ((t1.id), ())) UNION (SELECT max(NULL::date) FILTER (WHERE COALESCE(t2.a, true)) AS c0, (abs(3::numeric) + abs(10.00::numeric)) AS c1, bool_or(t2.a) AS c2 FROM (t1 t2 JOIN t0 t3 ON false) GROUP BY t2.d HAVING (false) IS TRUE);

CREATE VIEW v1 AS SELECT CASE WHEN false THEN t2.a ELSE (t2.a * (t2.id)::numeric) END AS c0, (((t2.id)::int2 + 10::int2) + 3::int2) AS c1, '{"b": [1, "a"]}'::json AS c2 FROM ((VALUES ('bar', false, true)) s1(v0, v1, v2) JOIN t0 t2 ON s1.v0 >= ALL (SELECT s1.v0 AS c0 FROM v0 t3 WHERE t3.c2)) WHERE (jsonb_array_length(jsonb_path_query_array(jsonb_strip_nulls('1'::jsonb), '$.*'))) IN (extract(day from '1999-08-17'::date)::int);

INSERT INTO t1 AS x (a, b, c, d, e) VALUES (NULL, DEFAULT, '1 day', 0.25, DEFAULT), (true, -1, NULL, DEFAULT, 'ab'), (true, 0, DEFAULT, 2, DEFAULT) RETURNING 1e3::numeric;

UPDATE t0 x SET a = round(mod(12345678.9::numeric, 3), 3), b = abs(11::int8) WHERE (7, upper(substr('é', 0, 2))) = (width_bucket(x.a, 0, 10, 4), split_part(('1'::jsonb ->> 'b'), ',', 1)) RETURNING concat_ws('-', split_part('a', ',', 1), format('%s|%s', 'B', 7)), round(x.a, 2);

DELETE FROM t0 x WHERE (('0'::interval + '-2 days'::interval) + ('1 year 2 days'::interval * x.id)) < justify_hours('0'::interval) RETURNING EXISTS (SELECT LEAST((('1999-01-14'::date - NULL::date) / x.id), t2.id) FROM (v1 t1 LEFT JOIN t0 t2 ON false) WHERE '{"b": [1, "a"]}'::jsonb <> '{"b": [1, "a"]}'::jsonb), '';

INSERT INTO t0 AS x (id, a, b) VALUES (19, NULL, 4), (51, 9.50, NULL), (4, NULL, 14) RETURNING x.a, (SELECT (t1.a)::numeric(6,1) FROM t0 t1 ORDER BY 1 LIMIT 1);

INSERT INTO t0 AS x (id, a, b) SELECT ascii(format('%s|%s', COALESCE(t1.e, t1.e), ('null'::jsonb::jsonb ->> 'a')::int)), mod(mod((t2.c1)::text::numeric, 3), 3), 3::int8 AS c2 FROM (t1 t1 LEFT JOIN v0 t2 ON t1.a = t2.c2) RETURNING '2025-05-10'::date = ('2000-03-06'::date - (-3));

DELETE FROM t1 x WHERE (GREATEST('2024-06-19 07:00:00'::timestamp, '2024-01-21 19:00:00'::timestamp) - ('2025-10-25 11:59:00'::timestamp - '1999-05-15 17:00:00'::timestamp)) <> (('2000-04-28'::date)::text::date)::timestamp RETURNING x.*;

DO $$ BEGIN RAISE NOTICE 'v=%', md5(initcap(NULL::text)); END $$;

UPDATE t1 x SET a = (NULL::jsonb - 'a') ?| ARRAY['a', 'x'] RETURNING (NULLIF(x.a, x.a))::text, x.b;

UPDATE t0 x SET b = abs((x.b * (-1)));

DO $$ BEGIN RAISE NOTICE 'v=%', 'Ab'; END $$;

UPDATE t0 x SET a = mod(100::numeric, 3), b = (x.b * (x.id & x.id)) WHERE true RETURNING x.*;

UPDATE t0 x SET b = 6::int8;

DO $$ BEGIN RAISE NOTICE 'v=%', substr(array_to_string('{"x y",z}'::text[], ',', '?'), 0, 1); END $$;

INSERT INTO t0 AS x (id, a, b) SELECT (jsonb_array_length(('{"a": {"b": 2}}'::jsonb || '[1, 2, 3]'::jsonb)) - abs((s2.u - s2.u))), t1.c0, s2.o FROM (v1 t1 JOIN unnest('{1,2}'::int4[]) WITH ORDINALITY s2(u, o) ON true) RETURNING md5(('{"b": [1, "a"]}'::jsonb ->> 'a'));

-- statement
SELECT (COALESCE(('{5}'::int4[] || s2.g), ('{NULL,3}'::int4[] || '{1,1,2}'::int4[])) || (('{2,NULL}'::int4[] || s2.g) || '{2,NULL}'::int4[])) AS c0, lpad(COALESCE(substr('bar', 3, 0), lpad('9', 3, '*')), 3, '*') AS c1, string_to_array((SELECT quote_literal('z') FROM v0 t4 WHERE NULL::bool ORDER BY 1 LIMIT 1), ',') AS c2, repeat('A', 2) AS c3 FROM ((v0 t1 JOIN generate_series(1, 2) s2(g) ON abs(3::int8) IN (abs(1::int8))) LEFT JOIN v0 t3 ON t1.c0 = t3.c0) WHERE 5::int8 >= abs((3::int8 + 10::int8)) ORDER BY 1 NULLS FIRST, 2, 3, 4 FOR KEY SHARE;
