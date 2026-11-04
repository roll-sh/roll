-- class: wrong_error
-- ordered: false
-- note: 42P10 vs 42725
-- engine: ERROR 42P10: GROUP BY position 6 is not in select list
-- engine: POSITION: 1002
-- reference: ERROR 42725: function array_agg(unknown) is not unique
-- reference: HINT: Could not choose a best candidate function. You might need to add explicit type casts.
-- reference: POSITION: 932

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

INSERT INTO t1 AS x (a, b, c, d) VALUES ('{a,b}', DEFAULT, DEFAULT, '2024-01-24') RETURNING CASE WHEN false THEN (SELECT NULL::numeric FROM (SELECT DISTINCT ((justify_hours(t2.b) + (t2.b + t2.b)) + t2.b) AS c0, CASE WHEN true THEN ARRAY[array_to_string(x.a, ',', '?'), ('{5}'::int4[])::text] ELSE CASE WHEN false THEN array_append(x.a, t2.c) ELSE ARRAY[t2.c, t2.c] END END AS c1, array_append(ARRAY[initcap(t2.c), t2.c], regexp_replace((t2.a ->> 'a'), '[ab]', 'y', 'g')) AS c2, '2024-09-04 12:00:00+02:30'::timestamptz AS c3 FROM ONLY t0 t2 WHERE NULLIF(COALESCE(true, false), t2.id IS NOT NULL)) s1 WHERE true ORDER BY 1 LIMIT 1) ELSE (99.99::numeric + 0.001::numeric) END, extract(day from (x.d - 7))::int;

INSERT INTO t2 AS x (id, a, b, c, d) SELECT t1.d, '{"a": 1, "b": "x"}'::json AS c1, (t1.a - 'a'), '2024-05-10 21:00:00+00'::timestamptz, -1::int2 FROM t0 t1 RETURNING 'aa';

DELETE FROM t1 x USING t2 u WHERE false;

DO $$ BEGIN RAISE NOTICE 'v=%', array_to_string('{}'::text[], ',', '?'); END $$;

-- statement
WITH w1 AS (SELECT 4::int8 AS c0, (CASE WHEN false THEN (SELECT max(('1999-11-11'::date)::timestamp) FROM t3 t7 WHERE t7.a IN (abs(t7.a), (2::int8 + t7.a), abs(t7.a))) ELSE '2024-03-15 09:59:00'::timestamp END - NULL::timestamp) AS c1, '{}'::json AS c2 FROM (SELECT ceil(round(1e3::numeric, 3)) AS c0, max(repeat(quote_literal('a'), 2)) FILTER (WHERE t3.b < t3.b) AS c1, NULL::text AS c2, max(t3.d) AS c3 FROM t2 t3 WHERE (lower('a,b') COLLATE "en-x-icu") NOT BETWEEN md5(CASE WHEN true THEN 'ab' ELSE 'abc' END) AND md5('') GROUP BY GROUPING SETS ((t3.b), (t3.id))) s2 CROSS JOIN LATERAL (SELECT (CASE WHEN false THEN '{}'::int4[] ELSE (NULL::int4[] || (-4)) END)[1:2] AS c0, left(quote_literal(t6.c), -1) AS c1, t5.b AS c2, ((date_trunc('week', '2024-04-23 21:30:00'::timestamp) AT TIME ZONE 'UTC') + t6.b) AS c3 FROM (t1 t5 JOIN t0 t6 ON t5.b = t6.c AND true)) s4) SELECT '[1, 2, 3]'::json AS c0, jsonb_agg(3 ORDER BY 3) AS c1, array_agg('Z' ORDER BY 'Z') AS c2 FROM w1 t8 , w1 t9 GROUP BY t9.c1, (6);
