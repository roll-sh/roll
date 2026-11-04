-- class: error_detail
-- ordered: true
-- note: 42P01
-- engine: ERROR 42P01: invalid reference to FROM-clause entry for table "t1"
-- engine: DETAIL: There is an entry for table "t1", but it cannot be referenced from this part of the query.
-- engine: HINT: To reference that table, you must mark this subquery with LATERAL.
-- engine: POSITION: 97
-- reference: ERROR 42P01: invalid reference to FROM-clause entry for table "t1"
-- reference: DETAIL: There is an entry for table "t1", but it cannot be referenced from this part of the query.
-- reference: POSITION: 97

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

-- statement
(SELECT s2.v0 AS c0, (t1.a SIMILAR TO 'a%')::text AS c1 FROM (t0 t1 RIGHT JOIN (VALUES ('ba' >= t1.b), (false)) s2(v0) ON ascii(t1.a) IN (SELECT (jsonb_array_length(('"s"'::jsonb || '1'::jsonb)) / (CASE WHEN true THEN '[1, 2, 3]'::jsonb ELSE '"s"'::jsonb END::jsonb ->> 'a')::int) FROM (t1 t3 LEFT JOIN t0 t4 ON s2.v0))) WHERE mod((10)::numeric, 3) <= round(('NaN'::numeric / 2.5), 0)) EXCEPT ALL (SELECT bool_and(lpad('10', 3, '*') IN (SELECT 'x ' AS c0 FROM t1 t8 CROSS JOIN t1 t9 WHERE COALESCE(1.5::numeric, abs(99.99::numeric)) < NULLIF(ceil(12345678.9::numeric), 0::numeric))) FILTER (WHERE false) AS c0, min(split_part(concat('Z', NULL::int4), ',', 1)) AS c1 FROM t1 t5 WHERE abs(8::int8) IN (SELECT (min(11::int8) * t5.c) AS c0 FROM (t0 t6 FULL JOIN t1 t7 ON (NULL::date - t7.c) <> ('2025-01-27'::date - t7.c))) GROUP BY CUBE (t5.id, t5.a)) ORDER BY 1, 2 LIMIT 1;
