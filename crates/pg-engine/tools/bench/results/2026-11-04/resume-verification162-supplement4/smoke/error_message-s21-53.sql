-- class: error_message
-- ordered: true
-- note: cannot delete from scalar | cannot get array length of a non-array
-- engine: ERROR 22023: cannot delete from scalar
-- reference: ERROR 22023: cannot get array length of a non-array

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

UPDATE t2 x SET b = x.b RETURNING (NULL::numeric / 7), (extract(day from '2025-08-10'::date)::int % 1);

DELETE FROM t0 x USING t1 u WHERE x.b = u.d;

-- statement
(SELECT mode() WITHIN GROUP (ORDER BY (-0.25::numeric * t1.c)) AS c0, (jsonb_array_length('{"b": [1, "a"]}'::jsonb) - position('ß' in 'foo')) AS c1, jsonb_build_object('k', (-5), 'v', '10') IS DISTINCT FROM jsonb_strip_nulls('{"a": null}'::jsonb) AS c2 FROM t0 t1 WHERE NULLIF(jsonb_path_query_first(t1.a, '$.a'), NULLIF(t1.a, t1.a)) <> (jsonb_strip_nulls('"s"'::jsonb) - 'a')) UNION (SELECT round(round(COALESCE(0.001::numeric, 1.5::numeric), 0), 3) AS c0, ((COALESCE(NULL::date, '1999-05-26'::date) + t2.id) - make_date(2024, 3, 1)) AS c1, to_char((0.001::numeric * 0::numeric), 'FM999.00') IN (trim('10'), 'aa') AS c2 FROM (t2 t2 LEFT JOIN v0 t3 ON t2.id = t3.c2 AND '2000-06-07 20:59:00'::timestamp >= NULL::timestamp) WHERE true) ORDER BY 1, 2, 3 DESC NULLS LAST LIMIT 5;
