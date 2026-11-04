-- class: error_message
-- ordered: false
-- note: aggregate functions are not allowed in WHERE | aggregate functions are not allowed in FROM clause of their own query level
-- engine: ERROR 42803: aggregate functions are not allowed in WHERE
-- engine: POSITION: 1658
-- reference: ERROR 42803: aggregate functions are not allowed in FROM clause of their own query level
-- reference: POSITION: 1044

CREATE TABLE t0 (a jsonb, b int4, c numeric(10,2), d text);

INSERT INTO t0 (a, b, c, d) VALUES ('{"a": 1, "b": "x"}', 8, 10.50, 'a%'), ('null', 0, 0.00, 'foo'), ('{"a": 3}', NULL, 6.50, 'Ä'), ('null', 3, NULL, 'ab'), (NULL, 9, 6.25, NULL), ('{"a": {"b": 2}}', -2, NULL, NULL), ('{"b": [1, "a"]}', 5, NULL, 'ba'), ('{"a": 3}', 1, NULL, 'b'), ('{"a": "1"}', -2, -1.50, 'é'), ('{"b": [1, "a"]}', 5, 8.00, 'abc'), ('"s"', 7, NULL, 'Ä'), ('null', 4, NULL, ' x');

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamp, b bool, c float8, d int4);

INSERT INTO t1 (id, a, b, c, d) VALUES (15, '2024-01-02 12:00:00', true, -0.75, 4), (22, '2024-01-06 06:30:00', NULL, -0.25, 5), (17, '2024-01-02 12:30:00', false, 0.75, 7);

CREATE INDEX t1_i191 ON t1 USING hash (d);

CREATE TABLE t2 (id int4 PRIMARY KEY, a int4, b int8, c timestamptz);

INSERT INTO t2 (id, a, b, c) VALUES (14, 6, NULL, NULL), (15, NULL, NULL, '2024-01-09 06:00:00+00'), (31, 5, 18, '2024-01-06 12:00:00+00'), (40, 5, 6, NULL), (12, 5, 2, '2024-01-01 12:00:00+00'), (3, -1, 10, '2024-01-04 12:00:00+00'), (35, 2, 3, NULL), (21, 4, 10, '2024-01-05 23:00:00+00'), (19, 9, -1, '2024-01-08 00:00:00+00'), (33, 9, 10, '2024-01-07 23:00:00+00'), (8, 2, NULL, '2024-01-08 12:00:00+00'), (38, 8, NULL, '2024-01-02 00:00:00+00'), (57, NULL, 3, '2024-01-02 06:00:00+00'), (5, 3, 12, '2024-01-08 23:00:00+00'), (25, 4, 5, '2024-01-04 12:00:00+00'), (2, 3, 6, '2024-01-09 12:00:00+00'), (20, NULL, 20, '2024-01-08 06:00:00+00'), (39, -1, 10, '2024-01-09 06:00:00+00'), (6, 6, NULL, '2024-01-06 23:00:00+00'), (36, 5, 18, NULL), (24, 0, NULL, '2024-01-02 23:00:00+00'), (10, 6, NULL, '2024-01-06 06:00:00+00'), (4, 2, 17, '2024-01-06 12:00:00+00'), (49, 0, 20, '2024-01-06 12:00:00+00'), (17, 0, 17, '2024-01-02 00:00:00+00'), (18, 0, 16, '2024-01-02 00:00:00+00'), (32, NULL, 20, '2024-01-02 12:00:00+00'), (23, 4, 19, '2024-01-08 00:00:00+00'), (45, NULL, 18, '2024-01-01 06:00:00+00'), (9, 7, NULL, '2024-01-03 00:00:00+00');

CREATE INDEX t2_i587 ON t2 USING hash (b);

CREATE INDEX t2_i319 ON t2 (id);

ANALYZE;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT b, d FROM t0 WHERE b <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT b, d FROM t0 WHERE b IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT isfinite(t1.a) AS c0, t1.b AS c1, length(' x') AS c2, '{1,1,2}'::int4[] AS c3 FROM t1 t1 CROSS JOIN unnest('{NULL,3}'::int4[]) WITH ORDINALITY s2(u, o) WHERE false;

CREATE VIEW v1 AS SELECT jsonb_strip_nulls(('[1, 2, 3]'::jsonb - 'a')) AS c0, NULL::numeric AS c1, array_append(array_append(array_append('{A,a}'::text[], 'z'), left(' x', 1)), 'a_b') AS c2 FROM t1 t1;

UPDATE t0 x SET d = x.d, a = x.a WHERE abs(extract(day from '2000-12-20'::date)::int) BETWEEN (x.b + COALESCE(x.b, x.b)) AND CASE x.b WHEN 4 THEN array_length('{NULL,3}'::int4[], 1) WHEN 2 THEN (x.c)::int4 END;

-- statement
WITH w1 AS NOT MATERIALIZED (SELECT left(COALESCE('ab', (s4.c1)::text), 2) AS c0, '2024-10-02 04:00:00+02:30'::timestamptz AS c1 FROM ((SELECT NULL::timestamp AS c0 FROM ONLY v0 t3) s2 RIGHT JOIN LATERAL (SELECT '-2 days'::interval AS c0, (12345678.9::numeric - num_nonnulls(t5.id, CASE WHEN false THEN 'Z' ELSE 'a%' END)) AS c1, ((NULL::int4[])[1:2] || ('2025-05-09'::date - '1999-01-15'::date)) AS c2 FROM ONLY t2 t5 WHERE false) s4 ON false) WHERE true) SELECT percentile_disc(0.25) WITHIN GROUP (ORDER BY CASE WHEN false THEN NULLIF('NaN'::numeric, 2.50::numeric) ELSE 1.5::numeric END) AS c0, count('{"a": 1, "b": "x"}'::json) AS c1, min('2000-12-20 18:00:00+00'::timestamptz) FILTER (WHERE NULL::bool) AS c2, string_agg(s8.c2, ',' ORDER BY s8.c2) FILTER (WHERE true) AS c3 FROM ((w1 t6 LEFT JOIN t2 t7 ON isfinite('2024-10-03'::date)) LEFT JOIN LATERAL (SELECT bit_or(s10.c1) FILTER (WHERE s9.v1) AS c0, (('2024-03-23 17:30:00'::timestamp + '3 days 04:00:00'::interval) - ('1999-05-08 21:00:00'::timestamp - '2 hours'::interval)) AS c1, min(substr(t6.c0, 2, 0)) AS c2, array_agg(s10.c1 ORDER BY s10.c1 NULLS FIRST) AS c3 FROM ((VALUES (99.99::numeric, false, 10), (1.5::numeric, false, 4), (-7.125::numeric, true, 2147483647), ('NaN'::numeric, false, 9)) s9(v0, v1, v2) JOIN (SELECT t12.c0 <> ALL (ARRAY[lower('ß'), concat(t12.c0, t11.a)]) AS c0, (- ((NULL::int4[] || '{3,1,2}'::int4[]))[1]) AS c1, round(12345678.9::numeric, 2) AS c2 FROM (t2 t11 FULL JOIN w1 t12 ON (false)::text >= array_to_string('{b,NULL}'::text[], ',', '?') COLLATE "en-x-icu") WHERE COALESCE((- 5) <> NULL::int2, false)) s10 ON NULLIF(t7.b, 9::int8) > (6)::int8) WHERE (SELECT bool_and(s10.c0) FILTER (WHERE '3 days 04:00:00'::interval <= '-2 days'::interval) FROM v1 t13 WHERE s9.v1) GROUP BY s9.v0) s8 ON t6.c0 = s8.c2) WHERE 'foo' > repeat(t6.c0, 2) HAVING 1::int8 IN (SELECT NULLIF((t7.b + t7.b), (s14.g)::int8) FROM generate_series(1, 0) s14(g) GROUP BY s14.g ORDER BY 1 OFFSET 1);
