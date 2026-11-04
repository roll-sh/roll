-- class: eval_error
-- ordered: true
-- note: ok vs 2201G: operand, lower bound, and upper bound cannot be NaN
-- engine: c0:25 | c1:16 | c2:23
-- engine: -- SELECT 0
-- reference: ERROR 2201G: operand, lower bound, and upper bound cannot be NaN

CREATE TABLE t0 (id int4 PRIMARY KEY, a timestamp, b text, c timestamp, d int4[]);

INSERT INTO t0 (id, a, b, c, d) VALUES (22, '2024-01-09 23:30:00', 'abc', '2024-01-02 00:30:00', '{3,1,2}'), (8, '2024-01-04 12:00:00', 'a%', '2024-01-06 23:30:00', '{3,1,2}'), (3, '2024-01-02 23:00:00', 'ba', NULL, NULL);

CREATE INDEX t0_i968 ON t0 (a);

CREATE INDEX t0_i562 ON t0 (a);

CREATE TABLE t1 (id int4, a text, b int4, c int4, d int8, e int2 DEFAULT 1) PARTITION BY LIST (a);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES IN ('a', 'b', 'A');

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

CREATE INDEX t1_i273 ON t1 (a, b);

CREATE INDEX t1_i434 ON t1 (d DESC NULLS LAST);

CREATE INDEX t1_i337 ON t1 (id) WHERE id IS NOT NULL;

CREATE TABLE t2 (id int4 PRIMARY KEY, a float8, b json);

INSERT INTO t2 (id, a, b) VALUES (11, 3.5, '"s"'), (13, NULL, '{"b": [1, "a"]}'), (19, -1.5, '{"a": "1"}'), (33, -1.25, '{"a": "1"}'), (7, -1.5, '[1, 2, 3]'), (3, NULL, '[1, 2, 3]'), (26, 2.5, '{"a": {"b": 2}}'), (36, 2.75, '{"a": "1"}'), (21, 1.5, 'null'), (25, 3.25, '{"a": "1"}'), (37, 0.5, '{"a": 1, "b": "x"}'), (30, 2.75, '{"b": [1, "a"]}');

CREATE INDEX t2_i408 ON t2 (a);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF ('{"b": [1, "a"]}'::jsonb || '"s"'::jsonb) IS NOT DISTINCT FROM '"s"'::jsonb THEN
    r := extract(day from '1999-05-27'::date)::int;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce((3::int8)::int4, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN isfinite(date_bin('7 days', '2025-09-10 00:59:00'::timestamp, '2000-01-01')::date) THEN RETURN (SELECT (count(*))::int FROM t2 t1);
  ELSE RETURN (date_part('month', ('1999-07-02 11:30:00'::timestamp - '1 year 2 days'::interval)))::int;
  END CASE;
END
$$;

DO $$ BEGIN RAISE NOTICE 'v=%', upper(array_to_string('{a,b}'::text[], ',', '?')); END $$;

INSERT INTO t1 AS x (id, a, b, c, d, e) VALUES (50, ' x', 7, 8, DEFAULT, DEFAULT), (14, 'ab', 3, 8, 15, 0), (51, DEFAULT, NULL, 8, 10, 0) RETURNING 1e3::numeric, extract(day from ('1999-11-22'::date + 6))::int;

MERGE INTO t0 x USING (SELECT id FROM t2 WHERE id % 1 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET c = (('2000-12-28'::date - NULL::int4))::timestamp WHEN NOT MATCHED THEN INSERT (id, a, b, c, d) VALUES (s.id + 100, '2024-01-09 23:00:00', NULL, DEFAULT, DEFAULT) RETURNING merge_action(), x.*;

ANALYZE t1;

INSERT INTO t0 AS x (id, a, b, c, d) SELECT (length(t1.a) - COALESCE(CASE WHEN false THEN t1.c ELSE t1.b END, NULLIF(t1.c, 7))) AS c0, date_trunc('hour', (SELECT ('1999-07-13'::date)::timestamp FROM t1 t3 ORDER BY 1 LIMIT 1)) AS c1, t1.a, GREATEST('2025-06-26 21:59:00'::timestamp, (GREATEST('2000-05-15 04:59:00'::timestamp, '2025-07-12 12:00:00'::timestamp) + (SELECT max(('1 mon -1 day'::interval + '1 year 2 days'::interval)) FROM t1 t4 WHERE NULL::bool))) AS c3, array_remove((('{}'::int4[] || t1.b))::text::int4[], t1.b) FROM t1 t1 LEFT JOIN t2 t2 USING (id) WHERE starts_with(t1.a, t1.a) ON CONFLICT DO NOTHING RETURNING LEAST(0::numeric, round(1::numeric, 0));

INSERT INTO t2 AS x (id, a, b) SELECT count((('2025-06-21 23:30:00'::timestamp - NULL::interval) AT TIME ZONE 'UTC')) AS c0, max(0.5::float8) FILTER (WHERE (SELECT bool_or(t6.b <> 'a') FROM t0 t6 WHERE true)), '{"a": 1, "b": "x"}'::json AS c2 FROM ((SELECT ('2025-01-26 09:59:00'::timestamp AT TIME ZONE 'UTC') AS c0, mod(mod(2.50::numeric, 3), 3) AS c1, array_append(ARRAY[(t2.id)::text, t3.a], concat_ws('-', replace('a_b', 'a', 'x'), quote_literal(t3.a))) AS c2 FROM t2 t2 LEFT JOIN t1 t3 USING (id) WHERE NULL::bool) s1 FULL JOIN t0 t4 ON (SELECT every(t5.id IN (t5.id)) FROM t0 t5)) WHERE array_append(array_append(s1.c2, t4.b), substr('aa', 0, 3)) @> array_append(array_append(s1.c2, 'Ä'), t4.b) GROUP BY t4.c, t4.id, (('1999-03-04'::date - NULL::date)) ON CONFLICT (id) DO UPDATE SET a = ((- x.a) * (x.a * x.a)) WHERE CASE WHEN false THEN NULL::bool ELSE false END RETURNING array_to_string(ARRAY['x ', 'abc'], ',', '?');

ANALYZE t1;

ANALYZE t0;

ANALYZE t1;

-- statement
WITH w1 AS NOT MATERIALIZED (SELECT DISTINCT mod(mod((s2.g)::numeric, 3), 3) AS c0, 'NaN'::float8 AS c1, s2.g AS c2 FROM generate_series(1, 5) s2(g)) SELECT lower((('2025-01-21'::date)::timestamp)::text) AS c0, (COALESCE('ab' LIKE '_', '2024-08-05'::date = '1999-04-06'::date) OR true) AS c1, ((NULL::date + s4.c0) - ('2000-03-20 02:30:00'::timestamp)::date) AS c2 FROM (w1 t3 LEFT JOIN (SELECT width_bucket(ceil(('NaN'::numeric + 12345678.9::numeric)), 0, 10, 4) AS c0, t5.id AS c1, CASE WHEN true THEN t5.id ELSE (t5.id + COALESCE(t5.id, 12)) END AS c2, ceil(ceil(round(0.001::numeric, 3))) AS c3 FROM t0 t5) s4 ON t3.c2 = s4.c1) WHERE EXISTS (SELECT 3 AS c0 FROM ((SELECT DISTINCT 'a' AS c0, abs(6::int8) AS c1, (COALESCE((-2), cardinality('{5}'::int4[])))::numeric AS c2 FROM (w1 t7 LEFT JOIN t2 t8 ON t7.c1 = t8.a) WHERE false) s6 JOIN t1 t9 ON s6.c0 = t9.a) WHERE false ORDER BY 1 DESC) ORDER BY 1, 2, 3;
