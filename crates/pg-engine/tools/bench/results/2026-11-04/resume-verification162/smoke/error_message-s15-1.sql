-- class: error_message
-- ordered: false
-- note: subquery uses ungrouped column "s1.c0" from outer query | subquery uses ungrouped column "s3.v1" from outer query
-- engine: ERROR 42803: subquery uses ungrouped column "s1.c0" from outer query
-- engine: POSITION: 520
-- reference: ERROR 42803: subquery uses ungrouped column "s3.v1" from outer query
-- reference: POSITION: 400

CREATE TABLE t0 (id int4 PRIMARY KEY, a jsonb, b interval, c text COLLATE "POSIX", d int4);

INSERT INTO t0 (id, a, b, c, d) VALUES (38, '{"a": 1, "b": "x"}', '-1 day', 'foo', 6);

CREATE INDEX t0_i288 ON t0 USING hash (id);

CREATE INDEX t0_i157 ON t0 (d);

CREATE INDEX t0_i608 ON t0 USING gin (a);

CREATE TABLE t1 (a text[], b text COLLATE "en-x-icu", c int2, d date);

INSERT INTO t1 (a, b, c, d) VALUES (NULL, 'ab', 6, '2024-03-10'), ('{b,NULL}', 'ab', 0, '2024-01-11'), ('{a,b}', 'a%', 3, '2024-03-12'), (NULL, 'b', 4, NULL), ('{a,b}', 'x ', 4, '2024-01-28'), ('{z}', 'z', NULL, NULL), ('{z}', 'aa', NULL, '2024-03-10'), ('{b,NULL}', 'ab', 3, NULL);

CREATE INDEX t1_i310 ON t1 (a);

CREATE INDEX t1_i124 ON t1 (b);

CREATE TABLE t2 (id int4, a json, b jsonb, c timestamptz, d int2 DEFAULT 0);

INSERT INTO t2 (id, a, b, c, d) VALUES (23, NULL, NULL, '2024-01-05 12:00:00+00', 6), (15, '{"b": [1, "a"]}', '{"a": "1"}', '2024-01-01 06:00:00+00', 1), (36, '{"a": 3}', NULL, '2024-01-08 23:00:00+00', 0), (33, '{"a": "1"}', '{"a": {"b": 2}}', '2024-01-01 00:00:00+00', 1), (7, '1', 'null', '2024-01-09 23:00:00+00', 6), (34, '1', 'null', '2024-01-07 12:00:00+00', NULL), (26, '"s"', '[]', NULL, 1), (40, NULL, '1', '2024-01-01 00:00:00+00', NULL), (2, '"s"', 'null', '2024-01-02 06:00:00+00', 4), (35, '"s"', NULL, '2024-01-03 06:00:00+00', 5), (27, '"s"', '[]', '2024-01-01 23:00:00+00', NULL), (57, 'null', '{"a": {"b": 2}}', '2024-01-03 00:00:00+00', 6), (10, '{"a": 1, "b": "x"}', '1', '2024-01-04 00:00:00+00', 3), (44, 'null', 'null', '2024-01-02 06:00:00+00', 2), (12, '{"b": [1, "a"]}', '[1, 2, 3]', '2024-01-01 23:00:00+00', 0), (20, NULL, '1', NULL, 4), (3, '{"b": [1, "a"]}', '"s"', '2024-01-06 23:00:00+00', 4), (25, '"s"', '[1, 2, 3]', '2024-01-09 00:00:00+00', 3), (48, '{"a": 1, "b": "x"}', '{"a": 1, "b": "x"}', '2024-01-09 06:00:00+00', 5), (21, '1', '[1, 2, 3]', '2024-01-04 06:00:00+00', NULL), (17, '"s"', '1', '2024-01-06 23:00:00+00', 6), (53, NULL, NULL, '2024-01-06 06:00:00+00', 4), (6, '{"a": 3}', NULL, '2024-01-01 06:00:00+00', 4), (13, '{"a": 1, "b": "x"}', 'null', '2024-01-07 00:00:00+00', 3), (38, '[1, 2, 3]', '{"a": 1, "b": "x"}', NULL, 5), (9, '{"a": "1"}', '[1, 2, 3]', '2024-01-03 00:00:00+00', 5), (19, '{"a": "1"}', '"s"', NULL, NULL), (43, '{"a": 1, "b": "x"}', '"s"', '2024-01-08 00:00:00+00', 5), (4, '{"a": {"b": 2}}', '{"b": [1, "a"]}', '2024-01-01 23:00:00+00', 4), (11, '{"a": {"b": 2}}', NULL, '2024-01-01 12:00:00+00', 6);

CREATE INDEX t2_i847 ON t2 USING hash (c);

CREATE TABLE t3 (a int8, b text);

INSERT INTO t3 (a, b) VALUES (-1, 'foo'), (16, 'z'), (2, 'A'), (19, 'a,b'), (16, NULL), (14, 'foo'), (NULL, '10'), (NULL, 'B'), (11, 'foo'), (NULL, 'x '), (20, 'b'), (1, 'z'), (3, 'abc'), (NULL, 'z'), (12, 'foo'), (NULL, 'x '), (0, 'é'), (16, ''), (2, 'Ä'), (6, '10'), (0, 'a%'), (8, 'foo'), (1, 'b'), (15, 'ba'), (2, NULL), (-2, 'ab'), (14, 'B'), (2, NULL), (13, ' x'), (-2, 'x ');

CREATE INDEX t3_i911 ON t3 (a);

CREATE INDEX t3_i572 ON t3 (b, a);

CREATE INDEX t3_i603 ON t3 USING hash (b);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN false THEN RETURN ((p & p) + (regexp_instr(q, 'b')));
  ELSE RETURN p;
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, c FROM t0 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, c FROM t0 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT mode() WITHIN GROUP (ORDER BY (round(-7.125::numeric, 3) / 7)) AS c0 FROM ((VALUES ('bar', false, true)) s1(v0, v1, v2) JOIN t2 t2 ON s1.v0 >= ALL (SELECT max(array_to_string('{"x y",z}'::text[], ',', '?')) FILTER (WHERE (true AND s1.v2)) FROM t2 t3 WHERE NULL::bool GROUP BY t3.id)) GROUP BY t2.d HAVING true;

-- statement
SELECT jsonb_agg((s4.g & (s1.c2 * s1.c2)) ORDER BY (s4.g & (s1.c2 * s1.c2))) AS c0, extract(day from (SELECT min(s3.v1) FROM (SELECT DISTINCT replace(reverse('B'), 'a', 'x') NOT IN (('2024-08-01 08:00:00'::timestamp)::text, NULLIF('foo', '9'), quote_literal('ß')) AS c0, array_position((('{3,1,2}'::int4[])[1:2] || ('{NULL,3}'::int4[] || '{2,NULL}'::int4[])), (mod(1e3::numeric, 3))::int4) AS c1, ((s3.v1 - t8.id))::timestamp AS c2 FROM t2 t8 WHERE (COALESCE(t8.id, ascii(' x')), repeat(left('ab', 2), 2)) < (CASE WHEN s1.c0 THEN COALESCE(t8.id, t8.id) ELSE t8.id END, ' x')) s7 WHERE false))::int AS c1, every(s1.c2 <= (s1.c1)::int4) AS c2, max(ascii('x ')) AS c3 FROM ((SELECT NULLIF('[]'::jsonb, '"s"'::jsonb) ?| ARRAY['a', 'x'] AS c0, (abs(t2.a) + t2.a) AS c1, strpos(reverse(quote_literal(t2.b)), 'a') AS c2, 2::float8 <> abs((0)::float8) AS c3 FROM t3 t2 ORDER BY 1 NULLS FIRST, 2 DESC NULLS LAST, 3 DESC, 4 DESC NULLS LAST) s1 JOIN (VALUES (true, '2025-03-13'::date, true), (false, '2025-02-10'::date, true), (NULL, '2024-10-04'::date, (false) IS TRUE)) s3(v0, v1, v2) ON s1.c0 = s3.v0) , generate_series(1, s1.c2 % 5) s4(g) WHERE (SELECT bool_and((SELECT bool_or('NaN'::numeric IN (3::numeric)) FROM t2 t6 WHERE 0.125::float8 IS NOT NULL)) FROM t3 t5 WHERE md5(t5.b) ILIKE '%b') GROUP BY s4.g;
