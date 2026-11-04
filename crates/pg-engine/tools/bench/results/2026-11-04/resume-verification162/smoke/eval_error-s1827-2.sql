-- class: eval_error
-- ordered: false
-- note: 22023: cannot get array length of a non-array vs ok
-- engine: ERROR 22023: cannot get array length of a non-array
-- reference: c0:1082 | c1:1184 | c2:114
-- reference: -- SELECT 0

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
SELECT t1.c0 AS c0, '2000-05-25 15:00:00+02:30'::timestamptz AS c1, t2.c0 AS c2 FROM ((v0 t1 JOIN v1 t2 ON GREATEST(t2.c2, t2.c2) BETWEEN SYMMETRIC t2.c2 AND left(t2.c2, 1)) JOIN LATERAL (SELECT max((s4.v0)::text::date) AS c0 FROM (VALUES (NULL, 2.50::numeric, true), ('2024-04-26'::date, 1e3::numeric, true), ('2024-10-24'::date, 1.5::numeric, false), ('2024-03-22'::date, 1e3::numeric, false)) s4(v0, v1, v2) CROSS JOIN LATERAL (SELECT (t6.b / 3) AS c0, quote_literal(CASE WHEN s4.v2 THEN regexp_replace(t2.c2, '[ab]', 'y', 'g') ELSE t2.c2 END) AS c1, t2.c3 AS c2 FROM (t1 t6 LEFT JOIN t1 t7 ON NULL::bool)) s5 WHERE s5.c1 ~ '%b' GROUP BY s5.c1) s3 ON t1.c0 = s3.c0);
