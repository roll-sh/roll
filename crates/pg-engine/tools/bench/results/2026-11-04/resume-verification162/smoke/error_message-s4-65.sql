-- class: error_message
-- ordered: false
-- note: subquery uses ungrouped column "t11.a" from outer query | subquery uses ungrouped column "t12.n" from outer query
-- engine: ERROR 42803: subquery uses ungrouped column "t11.a" from outer query
-- reference: ERROR 42803: subquery uses ungrouped column "t12.n" from outer query
-- reference: POSITION: 1794

CREATE TABLE t0 (id int4 PRIMARY KEY, a int8, b timestamptz);

INSERT INTO t0 (id, a, b) VALUES (29, 18, '2024-01-09 23:00:00+00'), (19, NULL, NULL), (21, 19, '2024-01-08 00:00:00+00'), (24, 14, '2024-01-06 06:00:00+00'), (15, 17, '2024-01-05 12:00:00+00'), (40, 1, '2024-01-04 06:00:00+00'), (9, 9, '2024-01-09 12:00:00+00'), (51, 11, '2024-01-07 23:00:00+00'), (2, NULL, '2024-01-01 12:00:00+00'), (7, 19, '2024-01-03 00:00:00+00'), (28, 7, '2024-01-05 12:00:00+00'), (8, -2, '2024-01-04 23:00:00+00');

CREATE INDEX t0_i762 ON t0 (id DESC NULLS LAST);

CREATE TABLE t1 (id int4, a jsonb, b interval, c numeric(10,2));

INSERT INTO t1 (id, a, b, c) VALUES (17, '{"a": "1"}', '-1 day', 9.50), (27, 'null', '2 hours', NULL), (31, '"s"', '0', -1.05), (3, '{"a": "1"}', '-1 day', 11.25), (48, '1', '30 days', 4.25), (8, '"s"', '-1 day', NULL), (28, 'null', NULL, 3.00), (32, '{"a": 1, "b": "x"}', '1 day', NULL);

CREATE INDEX t1_i788 ON t1 USING hash (b);

CREATE INDEX t1_i422 ON t1 (c);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
DECLARE r int := 0;
BEGIN
  IF (SELECT bool_and(s1.v0) FROM (VALUES (false), (NULL)) s1(v0)) THEN
    r := (p + array_length('{NULL,3}'::int4[], 1));
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(((8::int8)::int4 & (p & p)), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN COALESCE('1'::jsonb ? 'k', true) THEN RETURN (('[1, 2, 3]'::jsonb || 'null'::jsonb)::jsonb ->> 'a')::int;
  ELSE RETURN array_length('{NULL,3}'::int4[], 1);
  END CASE;
END
$$;

CREATE VIEW v0 AS SELECT ((s1.v2)::int2 + 4::int2) AS c0, justify_hours('2 hours'::interval) AS c1 FROM (VALUES (false, NULL, 10), (NULL, NULL, 10), (NULL, NULL, 7)) s1(v0, v1, v2) WHERE false;

CREATE VIEW v1 AS SELECT jsonb_agg((- t1.id) ORDER BY (- t1.id)) AS c0 FROM t1 t1 WHERE false;

DO $$ BEGIN RAISE NOTICE 'v=%', quote_literal(('{}'::int4[])::text); END $$;

ANALYZE t1;

MERGE INTO t0 x USING (SELECT id, b FROM t0 WHERE id % 1 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET b = '2024-11-02 00:00:00-05'::timestamptz WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (s.id + 100, DEFAULT, DEFAULT) RETURNING merge_action(), x.*;

INSERT INTO t1 AS x (id, a, b, c) VALUES (59, '{"a": 3}', '0', DEFAULT), (53, DEFAULT, '1 day', -2.00);

UPDATE t1 x SET b = ((x.b * x.id) + '3 days 04:00:00'::interval) WHERE array_append('{A,a}'::text[], '10') IS NULL IS NOT NULL RETURNING x.*;

UPDATE t0 x SET a = ((2::int8 + 3::int8) + abs(x.a));

ANALYZE t1;

UPDATE t1 x SET b = x.b, c = round(ceil(10.00::numeric), 1) WHERE (array_to_string(NULL::text[], ',', '?') < 'abc') IS TRUE;

INSERT INTO t0 AS x (id, a, b) VALUES (2, 13, '2024-01-08 23:00:00+00'), (1, 19, '2024-01-04 00:00:00+00'), (41, 16, '2024-01-08 23:00:00+00');

UPDATE t1 x SET c = -0.25::numeric;

-- statement
WITH RECURSIVE r1(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + 1) % 7, x || n::text FROM r1 WHERE length(x) < 5), w2 AS (SELECT abs((abs(2::float8) + 3.75::float8)) AS c0, 'ba' AS c1 FROM (VALUES ('NaN'::numeric, NULL)) s3(v0, v1) WHERE EXISTS (SELECT array_length((('{1,2}'::int4[] || '{NULL,3}'::int4[]) || length('Ä')), 1) FROM v0 t4 WHERE starts_with('ab', array_to_string('{ab}'::text[], ',', '?')))), w5 AS (SELECT (sign(array_length('{2,NULL}'::int4[], 1))::int & t9.id) AS c0 FROM ((SELECT ('{5}'::int4[] || ('{NULL,3}'::int4[] || ('{NULL,3}'::int4[] || '{2,NULL}'::int4[]))) AS c0, (('2000-09-25'::date + array_length('{5}'::int4[], 1)) - 'infinity'::date) AS c1, abs(abs(trunc(10.00::numeric))) AS c2 FROM (r1 t7 JOIN w2 t8 ON t7.x = t8.c1) ORDER BY 1 DESC NULLS LAST, 2 DESC, 3 NULLS FIRST) s6 RIGHT JOIN t0 t9 ON s6.c1 = t9.id AND abs(t9.a) NOT IN (SELECT (CASE WHEN false THEN t9.id ELSE (-2147483648) END)::int8 AS c0 FROM v0 t10 WHERE sign(length('a%'))::int IS NOT NULL GROUP BY ROLLUP (t10.c0, t10.c1))) WHERE ('1999-12-01 11:00:00'::timestamp + ('3 days 04:00:00'::interval + '-2 days'::interval)) > '1999-05-09 01:30:00'::timestamp) SELECT 6 AS c0, mode() WITHIN GROUP (ORDER BY ((1::numeric)::numeric(6,1) / 2.5)) AS c1, min(t12.x) AS c2 FROM (t0 t11 LEFT JOIN r1 t12 ON t11.id = t12.n AND t11.id < t12.n) WHERE CASE WHEN (-2) IN (SELECT 2 AS c0 FROM v1 t13 WHERE EXISTS (SELECT ((('infinity'::date)::text::date - NULL::date) % 3) FROM w5 t14 WHERE array_remove(('{3,1,2}'::int4[] || '{2,NULL}'::int4[]), sign(t14.c0)::int) > ARRAY[(t11.a)::int4, array_length('{3,1,2}'::int4[], 1)])) THEN CASE WHEN false THEN t12.x ELSE 'x ' END ELSE quote_literal(t12.x) END = ANY (array_append(string_to_array(t12.x, ','), quote_literal(t12.x))) HAVING abs((-1)) IN (SELECT t12.n FROM (v1 t16 JOIN v1 t17 ON t16.c0 = t17.c0 AND isfinite(('2025-12-06 07:30:00'::timestamp)::date)) WHERE (abs(t11.a) + (2::int8 * t11.id)) IN ((SELECT t18.a AS c0 FROM t0 t18) EXCEPT (SELECT ((CASE t20.c0 WHEN 1 THEN t19.c0 WHEN 3 THEN t20.c0 END * t20.c0))::int8 AS c0 FROM w5 t19 CROSS JOIN w5 t20 WHERE false)));
