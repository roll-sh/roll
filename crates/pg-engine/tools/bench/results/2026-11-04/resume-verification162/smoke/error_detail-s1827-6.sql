-- class: error_detail
-- ordered: true
-- note: 42601
-- engine: ERROR 42601: syntax error at or near ","
-- engine: POSITION: 530
-- reference: ERROR 42601: syntax error at or near ","
-- reference: POSITION: 531

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b json);

INSERT INTO t0 (id, a, b) VALUES (18, 3, NULL), (17, 3, '[]'), (19, -2, '{"a": 3}'), (6, 5, 'null'), (16, 0, '{"a": {"b": 2}}'), (28, 7, '{"a": {"b": 2}}'), (27, 7, '{"a": "1"}'), (24, 2, '{"a": 1, "b": "x"}'), (12, 5, NULL), (4, 6, '{"a": {"b": 2}}'), (13, -1, '{"b": [1, "a"]}'), (56, 1, '{"a": 3}'), (11, -2, '[1, 2, 3]'), (23, 1, NULL), (31, 2, NULL), (36, 4, '{"a": {"b": 2}}'), (59, 2, '{"a": 3}'), (41, 9, '[1, 2, 3]'), (9, 5, NULL), (35, 9, '1'), (32, 4, '[1, 2, 3]'), (58, NULL, '{"a": "1"}'), (26, 1, '[1, 2, 3]'), (52, 9, '{"a": 3}'), (30, 8, '"s"'), (2, NULL, NULL), (7, 4, '{"b": [1, "a"]}'), (8, 6, '{"a": {"b": 2}}'), (53, 5, '{"a": "1"}'), (45, -1, '[1, 2, 3]');

CREATE TABLE t1 (id int4, a numeric(10,2), b numeric(10,2));

INSERT INTO t1 (id, a, b) VALUES (9, -3.25, 8.75);

CREATE INDEX t1_i720 ON t1 USING hash (a);

CREATE TABLE t2 (id int4 PRIMARY KEY, a int8, b float8, c numeric(10,2), d interval);

INSERT INTO t2 (id, a, b, c, d) VALUES (9, 16, -2, 7.00, '30 days');

CREATE TABLE t3 (id int4, a timestamptz, b int4, c json);

CREATE TABLE t3_c (extra int) INHERITS (t3);

INSERT INTO t3_c (id, a, b, c, extra) VALUES (527, '2024-01-05 12:00:00+00', 8, '[]', 0);

INSERT INTO t3_c (id, a, b, c, extra) VALUES (888, '2024-01-05 12:00:00+00', 0, '{"a": 1, "b": "x"}', 2);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF ('2025-07-17 03:30:00'::timestamp AT TIME ZONE 'UTC') <= '2024-04-02 22:00:00+02:30'::timestamptz THEN
    r := (SELECT count(*) FROM unnest('{NULL,3}'::int4[]) WITH ORDINALITY s1(u, o) WHERE false);
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(p, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT max(('2000-04-25 03:59:00'::timestamp)::date) AS c0, max(COALESCE('2000-04-21 12:59:00'::timestamp, '2000-02-07 08:30:00'::timestamp)) AS c1 FROM t3 t1 GROUP BY t1.id;

CREATE VIEW v1 AS SELECT '"s"'::json AS c0, t2.id AS c1, NULLIF(('ß' COLLATE "und-x-icu"), left(format('%s|%s', 'aa', t2.a), 0)) AS c2, (abs((SELECT abs((-5)::int8) FROM t1 t3 ORDER BY 1 LIMIT 1)) + ((t1.id)::int8 * jsonb_array_length('{"b": [1, "a"]}'::jsonb))) AS c3 FROM t1 t1 CROSS JOIN t0 t2 WHERE (((int4range(LEAST(strpos(upper('foo' COLLATE "und-x-icu"), 'a'), (width_bucket(t1.a, 0, 10, 4) / 4)), GREATEST(strpos(upper('foo' COLLATE "und-x-icu"), 'a'), (width_bucket(t1.a, 0, 10, 4) / 4))) * int4range(NULL, (width_bucket(t1.a, 0, 10, 4) / 4)))::int4multirange - int4range(LEAST(t2.a, position('A' in split_part('', ',', 1))), GREATEST(t2.a, position('A' in split_part('', ',', 1))))::int4multirange) @> (regexp_instr((NULL::int4[])::text, 'b')));

-- statement
WITH w1 AS NOT MATERIALIZED (SELECT (NULL::float8 * ((t2.c1)::float8 + (NULL::float8 * -0::float8))) AS c0, '00:30:00'::interval AS c1, (- ((s3.u)::float8 + cbrt(3.75::float8))) AS c2 FROM (v1 t2 JOIN unnest('{2,NULL}'::int4[]) WITH ORDINALITY s3(u, o) ON t2.c1 = s3.u AND ('{}'::int4multirange @> t2.c1)) WHERE NOT (true)) SELECT DISTINCT ON (1) COALESCE(('{2,NULL}'::int4[] || COALESCE(NULL::int4[], '{3,1,2}'::int4[])), (ARRAY[t4.id, t5.id] || ('{"a": null}'::jsonb::jsonb ->> 'a')::int)) AS c0, reverse('Ä') AS c1 FROM (t1 t4 , t1 t5 LEFT JOIN t3 t6 ON t4.id = t6.id AND t5.id <> t6.id AND width_bucket(t4.a, 0, 10, 4) IN (t6.b, NULLIF(t5.id, t6.id), t5.id, width_bucket(t5.a, 0, 10, 4))) ORDER BY 1, 2 NULLS FIRST;
