-- class: missing_error
-- ordered: true
-- note: 42803: column "s8.u" must appear in the GROUP BY clause or be used in an aggregate function
-- engine: c0:1082
-- engine: 1999-08-04
-- engine: -- SELECT 1
-- reference: ERROR 42803: column "s8.u" must appear in the GROUP BY clause or be used in an aggregate function
-- reference: POSITION: 495

CREATE TABLE t0 (a text COLLATE "POSIX", b text, c text);

INSERT INTO t0 (a, b, c) VALUES ('bar', '10', 'Ab');

CREATE TABLE t1 (id int4 PRIMARY KEY, a float8, b int2, c int4, d json);

INSERT INTO t1 (id, a, b, c, d) VALUES (39, -1.75, 4, -2, '[1, 2, 3]'), (33, 0.25, 5, 1, '[1, 2, 3]'), (13, 3.5, 6, 9, '{"a": {"b": 2}}'), (16, 2.25, 0, -2, '{"a": "1"}'), (12, -0.25, 4, 5, '{"a": 1, "b": "x"}'), (15, 0, 6, -1, '{"a": "1"}'), (18, -1.75, NULL, -2, '{"a": "1"}'), (2, 3.75, 0, 7, '{"a": 1, "b": "x"}'), (35, 1.25, 1, 9, NULL), (5, -1, 4, NULL, '{"b": [1, "a"]}'), (48, 2.75, NULL, -1, '{"a": 3}'), (36, 1.5, 3, 6, '1'), (27, -0.75, 1, 7, '"s"'), (22, 0.75, 6, 6, '{"a": "1"}'), (20, NULL, 3, 0, NULL), (28, 3.5, NULL, 7, NULL), (29, -1.5, 0, NULL, '{"b": [1, "a"]}'), (9, 2.25, 5, 9, '{"b": [1, "a"]}'), (6, 4, 3, 8, NULL), (1, 0.5, 5, 2, '[1, 2, 3]');

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN false THEN RETURN upper((int4range(LEAST(extract(day from '2025-06-15'::date)::int, ascii('a%')), GREATEST(extract(day from '2025-06-15'::date)::int, ascii('a%'))) + int4range(ascii('a%'), NULL)));
  ELSE RETURN array_length('{NULL,3}'::int4[], 1);
  END CASE;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN true THEN RETURN f0((p * 7), (SELECT max(trim('Z')) FILTER (WHERE NOT (true)) FROM ONLY t1 t1 WHERE (q >= q COLLATE "C") IS TRUE));
  ELSE RETURN p;
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id FROM t1 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id FROM t1 WHERE id IS NULL;
  END IF;
END
$$;

UPDATE t0 x SET a = substr(quote_literal(x.b), 2, 2) RETURNING x.*;

DO $$ BEGIN RAISE NOTICE 'v=%', replace('ß', 'a', 'x'); END $$;

UPDATE t1 x SET d = json_build_array((x.c & 3), trim('a,b')) WHERE NOT EXISTS (SELECT 1 FROM (SELECT num_nonnulls((- x.id), trim((t2.b COLLATE "C"))) AS c0 FROM (t0 t2 LEFT JOIN t0 t3 ON t2.a = t3.a)) s1 WHERE (s1.c0, 'a,b') > ((array_length('{5}'::int4[], 1) << 1), to_char((99.99::numeric)::numeric(6,1), 'FM999.00')));

INSERT INTO t0 AS x (a, b, c) VALUES (DEFAULT, 'A', 'A') RETURNING concat(NULLIF(x.a, 'abc'), 10);

ANALYZE t0;

DO $$ BEGIN RAISE NOTICE 'v=%', lpad(upper('x '), 3, '*'); END $$;

DELETE FROM t0 x WHERE isfinite(make_date(2024, 6, 1));

INSERT INTO t1 AS x (id, a, b, c, d) SELECT length(LEAST(COALESCE('z', ''), initcap('ab'))) AS c0, abs(-0::float8), (t1.b + t1.b), length('Ab') AS c3, (json_build_array(width_bucket('NaN'::numeric, 0, 10, 4), substr(NULL::text, 2, 2)) -> 'a') FROM t1 t1 WHERE (t1.id, reverse(' x')) < (t1.id, regexp_replace('Z', '[ab]', 'y', 'g')) RETURNING (- (- x.id)), initcap('a%');

INSERT INTO t0 AS x (a, b, c) VALUES ('é', 'z', DEFAULT), ('bar', 'foo', 'foo'), (DEFAULT, 'Ä', DEFAULT);

-- statement
WITH w1 AS MATERIALIZED (SELECT DISTINCT (SELECT '2000-05-05'::date FROM t1 t5 WHERE isfinite('2000-05-07'::date) ORDER BY 1 LIMIT 1) AS c0 FROM (t1 t2 JOIN t1 t3 ON true) WHERE CASE WHEN 7::int8 >= 6::int8 THEN initcap(' x') ELSE md5(NULL::text) END <> ALL (SELECT DISTINCT 'ba' AS c0 FROM t1 t4 WHERE false)) SELECT ('1999-08-06'::date + (-2)) AS c0 FROM (SELECT LEAST('{3,1,2}'::int4[], '{}'::int4[]) AS c0, array_append('{b,NULL}'::text[], split_part(LEAST(' x', 'bar'), ',', 1)) AS c1, (((s8.u)::numeric * (SELECT avg(DISTINCT (s8.o + s8.o)) FILTER (WHERE false) FROM t0 t9 WHERE true)) + abs(NULL::numeric)) AS c2, string_to_array(upper(('a_b' || 'A')), ',') AS c3 FROM (t1 t7 RIGHT JOIN unnest('{NULL,3}'::int4[]) WITH ORDINALITY s8(u, o) ON t7.c = s8.u) WHERE true) s6 WHERE NOT EXISTS (SELECT (-3) AS c0 FROM (w1 t10 JOIN (VALUES (1e3::numeric)) s11(v0) ON (4 + 5) <= jsonb_array_length(NULL::jsonb))) ORDER BY 1 DESC NULLS LAST;
