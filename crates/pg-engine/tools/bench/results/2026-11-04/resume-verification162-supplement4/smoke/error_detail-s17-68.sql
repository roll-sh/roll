-- class: error_detail
-- ordered: false
-- note: 42804
-- engine: ERROR 42804: argument of CASE/WHEN must be type boolean, not type text
-- engine: POSITION: 178
-- reference: ERROR 42804: argument of CASE/WHEN must be type boolean, not type text
-- reference: POSITION: 171

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b numeric(10,2));

INSERT INTO t0 (id, a, b) VALUES (26, 4, 7.75), (9, -2, -3.25), (15, 8, -3.05), (17, 3, 1.75), (39, 1, NULL), (11, 0, 10.00), (16, 0, NULL), (18, 1, -1.00), (19, 7, 10.75), (45, 7, 6.00), (36, 4, NULL), (33, 6, NULL), (2, 2, -3.00), (28, 0, NULL), (27, 9, -2.00), (30, 9, 8.05), (12, 0, NULL), (5, NULL, 6.50), (3, -2, 5.75), (32, 4, 5.50);

CREATE INDEX t0_i338 ON t0 USING hash (b);

CREATE INDEX t0_i980 ON t0 (id);

CREATE TABLE t1 (id int4 PRIMARY KEY, a text[], b int8 DEFAULT 0, c text);

CREATE INDEX t1_i520 ON t1 (b);

CREATE TABLE t2 (a jsonb, b int4, c int2, d text[], e numeric(10,2)) PARTITION BY RANGE (b);

CREATE TABLE t2_p0 PARTITION OF t2 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t2_p1 PARTITION OF t2 FOR VALUES FROM (3) TO (6);

CREATE TABLE t2_pd PARTITION OF t2 DEFAULT;

INSERT INTO t2 (a, b, c, d, e) VALUES ('null', 9, 2, '{A,a}', -2.50), ('{"a": "1"}', -1, NULL, '{}', 11.05), ('{"a": 1, "b": "x"}', NULL, 6, '{ab}', 3.05), ('{"a": "1"}', -1, 4, NULL, 3.25), (NULL, NULL, 4, '{z}', 5.50), ('{"a": {"b": 2}}', -1, 3, '{}', 4.50), ('null', 0, 5, NULL, 0.25), (NULL, 1, 5, '{z}', NULL), ('{"a": "1"}', 8, NULL, '{b,NULL}', 6.50), (NULL, 9, 3, '{ab}', 11.00), ('[1, 2, 3]', 9, 2, '{b,NULL}', 8.00), ('{"a": 1, "b": "x"}', 0, 2, '{A,a}', NULL);

CREATE TABLE t3 (id int4 PRIMARY KEY, a timestamptz, b int8, c interval, d text COLLATE "en-x-icu");

INSERT INTO t3 (id, a, b, c, d) VALUES (16, NULL, -2, '30 days', 'ß'), (29, '2024-01-07 12:00:00+00', NULL, '0', ' x'), (47, '2024-01-05 23:00:00+00', 9, '1 day', 'abc'), (24, '2024-01-09 06:00:00+00', -1, NULL, '10'), (32, '2024-01-01 23:00:00+00', NULL, NULL, '9'), (40, '2024-01-07 23:00:00+00', 12, '0', 'aa'), (25, '2024-01-05 06:00:00+00', NULL, '1 mon', 'Ä'), (4, '2024-01-03 00:00:00+00', 10, '24:00:00', 'Ab'), (27, NULL, 12, '1 mon', NULL), (26, '2024-01-09 12:00:00+00', 6, NULL, 'é'), (15, '2024-01-04 12:00:00+00', 0, '-1 day', '10'), (18, '2024-01-06 06:00:00+00', NULL, '2 hours', '');

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
DECLARE r int := 0;
BEGIN
  IF ('[]'::jsonb || '{"a": 2.50}'::jsonb) <> NULLIF('{"a": 1, "b": "x"}'::jsonb, '{"a": 2.50}'::jsonb) THEN
    r := CASE (5 - 12) WHEN 2 THEN NULLIF(p, p) WHEN 0 THEN (p * p) END;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(6, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF 12345678.9::numeric >= trunc(-7.125::numeric) THEN
    r := p;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce((cardinality('{1,2}'::int4[]) % 3), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, c FROM t1 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, c FROM t1 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT (count(*))::int AS c0, percentile_disc(0.25) WITHIN GROUP (ORDER BY round(round(t1.e, 1), 3)) FILTER (WHERE NULLIF(NULL::bool, true)) AS c1, json_build_array(1, NULLIF('a', 'a%')) AS c2 FROM t2 t1 WHERE false GROUP BY t1.e, t1.a HAVING (false OR 'Z' SIMILAR TO '%b');

UPDATE t2 x SET b = width_bucket(ceil(x.e), 0, 10, 4) WHERE (lower('A') || COALESCE('ß', 'x ')) <= array_to_string(x.d, ',', '?') RETURNING x.*;

WITH d AS (DELETE FROM t1 x WHERE ('1999-03-27'::date - '2000-10-15'::date) IN (abs(x.id), (- x.id), (- x.id), (x.id + 10)) RETURNING x.*) SELECT count(*) FROM d;

ANALYZE t3;

DELETE FROM t3 x USING t1 u WHERE x.b = u.b AND isfinite('2025-03-01'::date) RETURNING x.id;

INSERT INTO t1 AS x (id, a, b, c) VALUES (11, '{A,a}', 15, DEFAULT), (3, NULL, 15, DEFAULT) RETURNING CASE x.id WHEN 2 THEN ('{}'::int4[])[1] WHEN 2 THEN (x.id & 9) END, concat(repeat(x.c, 2), f0(x.id, x.c));

ANALYZE t1;

MERGE INTO t0 x USING (SELECT id FROM t1 WHERE id % 3 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET a = jsonb_array_length(('"s"'::jsonb - 'a')) WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (s.id + 100, DEFAULT, DEFAULT) RETURNING merge_action(), x.*;

DO $$ BEGIN RAISE NOTICE 'v=%', '9'; END $$;

DO $$ BEGIN RAISE NOTICE 'v=%', repeat(('b' COLLATE "POSIX"), 2); END $$;

ANALYZE t0;

MERGE INTO t1 x USING (SELECT id, b FROM t3 WHERE id % 2 = 0) s ON s.id = x.id WHEN MATCHED AND false THEN UPDATE SET b = (x.b)::text::int8 WHEN NOT MATCHED BY SOURCE THEN DELETE RETURNING merge_action(), x.*;

-- statement
SELECT NULL::text < 'B' COLLATE "POSIX" AS c0, (('2000-11-22'::date - '2025-02-09'::date))::numeric AS c1, '{}'::jsonb AS c2 FROM (((SELECT date_trunc('month', CASE WHEN NULLIF(s2.v1, s2.v1) THEN ('2025-04-18 18:59:00'::timestamp AT TIME ZONE 'UTC') ELSE '2000-01-20 18:00:00+02:30'::timestamptz END) AS c0 FROM (VALUES (1::numeric, NULL)) s2(v0, v1) WHERE isfinite(make_date(2024, 7, 1))) s1 JOIN t3 t3 ON s1.c0 = t3.a) LEFT JOIN t0 t4 ON t3.id = t4.id) WHERE false GROUP BY t3.a;
