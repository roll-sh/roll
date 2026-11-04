-- class: error_detail
-- ordered: false
-- note: 42725
-- engine: ERROR 42725: function array_agg(unknown) is not unique
-- engine: POSITION: 464
-- reference: ERROR 42725: function array_agg(unknown) is not unique
-- reference: HINT: Could not choose a best candidate function. You might need to add explicit type casts.
-- reference: POSITION: 464

CREATE TABLE t0 (id int4, a date, b date, c json, d timestamptz) PARTITION BY RANGE (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO ('2024-02-01');

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b, c, d) VALUES (9, '2024-01-14', '2024-02-04', '1', '2024-01-08 12:00:00+00'), (14, '2024-02-05', '2024-02-19', NULL, '2024-01-07 00:00:00+00'), (26, '2024-01-01', '2024-01-06', '"s"', '2024-01-01 06:00:00+00'), (29, '2024-01-12', NULL, 'null', '2024-01-02 06:00:00+00'), (8, '2024-02-14', NULL, NULL, '2024-01-09 12:00:00+00'), (10, '2024-01-23', '2024-01-19', '{"b": [1, "a"]}', '2024-01-07 23:00:00+00'), (48, '2024-01-27', '2024-03-25', 'null', '2024-01-02 00:00:00+00'), (3, NULL, NULL, '{"a": {"b": 2}}', '2024-01-02 23:00:00+00'), (11, '2024-01-27', '2024-02-04', NULL, '2024-01-05 00:00:00+00'), (27, '2024-03-03', '2024-02-13', '{"a": {"b": 2}}', '2024-01-04 00:00:00+00'), (2, '2024-01-28', '2024-03-19', NULL, NULL), (42, '2024-03-02', '2024-03-12', '{"a": {"b": 2}}', '2024-01-05 23:00:00+00'), (39, '2024-02-25', NULL, NULL, NULL), (6, NULL, '2024-02-19', '"s"', NULL), (44, '2024-02-27', '2024-02-06', '{"b": [1, "a"]}', '2024-01-03 12:00:00+00'), (7, '2024-03-19', '2024-03-11', '[]', '2024-01-06 23:00:00+00'), (33, '2024-03-10', '2024-02-23', NULL, '2024-01-01 00:00:00+00'), (36, '2024-01-01', '2024-02-27', '1', '2024-01-05 12:00:00+00'), (15, '2024-03-01', '2024-02-10', '{"a": 3}', NULL), (38, NULL, '2024-03-22', '"s"', '2024-01-03 23:00:00+00'), (51, '2024-03-09', '2024-02-14', '[1, 2, 3]', '2024-01-05 06:00:00+00'), (32, '2024-02-27', '2024-01-24', '{"b": [1, "a"]}', '2024-01-02 06:00:00+00'), (40, '2024-02-16', '2024-03-06', NULL, NULL), (17, NULL, '2024-03-03', '{"b": [1, "a"]}', '2024-01-09 00:00:00+00'), (21, '2024-01-11', '2024-03-20', '"s"', '2024-01-01 06:00:00+00'), (50, '2024-03-17', '2024-03-07', NULL, NULL), (25, '2024-02-14', '2024-03-04', '"s"', '2024-01-06 00:00:00+00'), (53, '2024-03-22', '2024-03-24', '[1, 2, 3]', '2024-01-07 00:00:00+00'), (19, '2024-02-26', NULL, '{"a": "1"}', '2024-01-01 00:00:00+00'), (35, '2024-01-12', '2024-02-24', '{"b": [1, "a"]}', '2024-01-03 06:00:00+00');

CREATE INDEX t0_i31 ON t0 (id) WHERE id IS NOT NULL;

CREATE INDEX t0_i137 ON t0 (a);

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamptz, b json, c text[], d json, e text);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (17, NULL, '{"b": [1, "a"]}', '{A,a}', '[1, 2, 3]', 'Ab'), (16, '2024-01-05 23:00:00+00', '1', '{z}', '[1, 2, 3]', 'ß'), (40, NULL, '1', '{A,a}', '"s"', 'a%'), (19, '2024-01-08 00:00:00+00', '{"a": "1"}', '{}', '1', 'a'), (7, NULL, '1', NULL, '1', NULL), (26, NULL, '{"a": "1"}', '{z}', '1', '10'), (6, NULL, '[]', '{}', NULL, 'é'), (30, '2024-01-04 23:00:00+00', '{"a": 3}', '{b,NULL}', NULL, NULL), (1, '2024-01-06 00:00:00+00', NULL, NULL, '1', 'B'), (28, '2024-01-04 06:00:00+00', '"s"', '{b,NULL}', '{"a": "1"}', 'x '), (33, '2024-01-03 12:00:00+00', '[1, 2, 3]', NULL, NULL, NULL), (43, '2024-01-02 00:00:00+00', 'null', '{ab}', NULL, NULL), (32, NULL, '1', '{z}', '"s"', 'a%'), (12, '2024-01-08 12:00:00+00', '[1, 2, 3]', '{ab}', '{"a": {"b": 2}}', 'é'), (39, '2024-01-07 06:00:00+00', '"s"', '{a,b}', '{"a": "1"}', 'Ab'), (35, '2024-01-04 00:00:00+00', 'null', '{A,a}', 'null', 'é'), (22, '2024-01-03 23:00:00+00', '[]', '{A,a}', '1', NULL), (36, '2024-01-02 06:00:00+00', 'null', '{z}', '[1, 2, 3]', 'ba'), (60, '2024-01-05 06:00:00+00', '{"a": 1, "b": "x"}', '{}', 'null', 'a'), (31, '2024-01-06 00:00:00+00', NULL, '{z}', '[1, 2, 3]', 'Z');

CREATE INDEX t1_i872 ON t1 (e) WHERE e IS NOT NULL;

CREATE TABLE t2 (id int4, a numeric(10,2), b int2, c json, d numeric(10,2));

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2_c (id, a, b, c, d, extra) VALUES (1096, NULL, 2, '{"a": 3}', 12.05, 1);

INSERT INTO t2_c (id, a, b, c, d, extra) VALUES (844, -1.00, 0, '{"a": {"b": 2}}', 0.00, 3);

INSERT INTO t2_c (id, a, b, c, d, extra) VALUES (414, 4.00, 5, '{"a": 3}', 11.50, 3);

INSERT INTO t2 (id, a, b, c, d) VALUES (24, 4.00, 3, NULL, 10.75), (9, -1.00, NULL, '{"a": "1"}', -1.05), (30, -3.75, 5, '{"a": 3}', 10.00);

CREATE INDEX t2_i326 ON t2 (a);

CREATE INDEX t2_i58 ON t2 (b);

CREATE TABLE t3 (a interval, b int4 DEFAULT 0, c numeric(10,2), d int4);

INSERT INTO t3 (a, b, c, d) VALUES ('2 hours', 4, 1.00, 8), ('24:00:00', 1, NULL, 0), (NULL, -2, NULL, 0), ('0', 0, 2.50, NULL), (NULL, 6, -3.00, -1), ('30 days', 5, 12.00, 9), ('30 days', NULL, NULL, NULL), (NULL, -2, 10.75, 7), ('1 mon', 6, NULL, -1), ('2 hours', -2, NULL, -1), ('30 days', 2, 0.25, 3), ('2 hours', NULL, 11.25, 4), ('0', 5, 12.75, 3), ('30 days', 2, 5.50, NULL), (NULL, 2, 12.25, 7), ('-1 day', 3, 7.00, 9), ('24:00:00', 8, 4.00, 3), ('1 mon', NULL, 0.05, -2), ('1 day', 6, 7.00, -2), ('30 days', 4, 9.75, 9), ('1 mon', 2, NULL, 2), ('24:00:00', 9, -1.75, 5), (NULL, 2, 9.00, NULL), ('30 days', 4, 11.05, 4), ('30 days', 5, -3.50, 4), ('2 hours', 5, -3.25, 1), ('2 hours', 6, -3.00, 7), ('30 days', NULL, 9.50, 9), (NULL, 8, 11.50, 5), ('0', 3, -1.00, 3);

CREATE INDEX t3_i623 ON t3 USING hash (c);

CREATE INDEX t3_i988 ON t3 USING hash (c);

CREATE INDEX t3_i97 ON t3 (a);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF to_jsonb(p) ?| ARRAY['a', 'x'] THEN
    r := sign((p * p))::int;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(((NULL::int8 + 9223372036854775807::int8))::int4, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

WITH RECURSIVE r(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + 2) % 7, x || n::text FROM r WHERE length(x) < 4) SEARCH BREADTH FIRST BY n SET ord SELECT n, x, ord FROM r;

UPDATE t2 x SET b = COALESCE((x.id)::int2, (x.id)::int2) WHERE true RETURNING x.*;

-- statement
WITH w1 AS ((SELECT t3.d AS c0, NOT (sign(t3.id)::int > ANY (('{1,1,2}'::int4[] || '{2,NULL}'::int4[]))) AS c1, format('%s|%s', replace(regexp_replace(t2.e, '[ab]', 'y', 'g'), 'a', 'x'), position(md5(t2.e) in repeat(t2.e, 2))) AS c2 FROM (t1 t2 RIGHT JOIN t2 t3 ON t2.id = t3.id)) INTERSECT (SELECT ((- 2147483647))::numeric AS c0, false AS c1, max(concat_ws('-', format('%s|%s', t4.e, t4.id), upper('a,b'))) AS c2 FROM t1 t4 WHERE false HAVING trim('10') < ANY (array_agg(' x' ORDER BY ' x')))), w5 AS MATERIALIZED (SELECT 5::int8 AS c0 FROM (t1 t6 JOIN LATERAL (SELECT json_build_array(COALESCE(t8.id, t8.id), md5('Z')) AS c0, '2 hours'::interval AS c1 FROM t0 t8) s7 ON array_append(NULL::text[], ' x') @> '{a,b}'::text[]) WHERE (t6.id + extract(day from '2024-11-13'::date)::int) IS NULL) SELECT (SELECT (t13.b)::numeric FROM w5 t14 WHERE false ORDER BY 1 LIMIT 1) AS c0, trim_scale((t13.c)::numeric(6,1)) AS c1, t13.c AS c2, array_length(('{3,1,2}'::int4[] || '{5}'::int4[]), 1) = ((t13.c + t13.c))::float8 AS c3 FROM (((SELECT t10.a AS c0 FROM t3 t10 , t1 t11) s9 JOIN w5 t12 ON true) RIGHT JOIN t3 t13 ON s9.c0 = t13.a);
