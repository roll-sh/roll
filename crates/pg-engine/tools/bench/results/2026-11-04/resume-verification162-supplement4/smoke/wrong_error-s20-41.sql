-- class: wrong_error
-- ordered: false
-- note: 42803 vs 42725
-- engine: ERROR 42803: aggregate functions are not allowed in GROUP BY
-- engine: POSITION: 371
-- reference: ERROR 42725: function array_agg(unknown) is not unique
-- reference: HINT: Could not choose a best candidate function. You might need to add explicit type casts.
-- reference: POSITION: 237

CREATE TABLE t0 (id int4 PRIMARY KEY, a jsonb, b text, c int8);

INSERT INTO t0 (id, a, b, c) VALUES (33, '{"b": [1, "a"]}', ' x', 6), (40, 'null', ' x', 18), (4, '{"a": {"b": 2}}', 'ß', 11), (6, '[1, 2, 3]', 'ab', NULL), (23, '[1, 2, 3]', NULL, 19), (35, NULL, 'z', 7), (13, NULL, 'A', 4), (14, '{"a": 3}', 'ß', 13), (36, '[1, 2, 3]', 'aa', 1), (18, 'null', '10', 1), (38, '[1, 2, 3]', ' x', 7), (7, '[]', '9', 12);

CREATE INDEX t0_i880 ON t0 (a);

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamptz, b timestamptz);

INSERT INTO t1 (id, a, b) VALUES (16, '2024-01-01 00:00:00+00', NULL), (7, '2024-01-01 00:00:00+00', '2024-01-09 06:00:00+00'), (35, NULL, '2024-01-01 23:00:00+00'), (27, '2024-01-07 00:00:00+00', '2024-01-03 00:00:00+00'), (8, NULL, '2024-01-02 23:00:00+00'), (29, '2024-01-06 00:00:00+00', '2024-01-07 23:00:00+00'), (10, '2024-01-09 23:00:00+00', '2024-01-02 12:00:00+00'), (4, '2024-01-07 06:00:00+00', '2024-01-06 06:00:00+00'), (1, '2024-01-04 06:00:00+00', '2024-01-09 12:00:00+00'), (41, '2024-01-09 00:00:00+00', '2024-01-04 12:00:00+00'), (11, '2024-01-06 00:00:00+00', '2024-01-07 00:00:00+00'), (38, '2024-01-02 23:00:00+00', '2024-01-08 23:00:00+00'), (5, '2024-01-06 06:00:00+00', '2024-01-01 23:00:00+00'), (24, NULL, '2024-01-07 23:00:00+00'), (49, '2024-01-01 23:00:00+00', '2024-01-02 00:00:00+00'), (25, '2024-01-03 23:00:00+00', '2024-01-01 06:00:00+00'), (22, '2024-01-03 12:00:00+00', '2024-01-04 00:00:00+00'), (36, '2024-01-06 12:00:00+00', '2024-01-04 12:00:00+00'), (28, '2024-01-05 00:00:00+00', '2024-01-01 23:00:00+00'), (56, '2024-01-06 23:00:00+00', '2024-01-02 23:00:00+00');

CREATE INDEX t1_i845 ON t1 (id);

CREATE INDEX t1_i734 ON t1 (b);

CREATE TABLE t2 (a int8, b timestamptz);

CREATE INDEX t2_i920 ON t2 (b);

CREATE INDEX t2_i671 ON t2 (a);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN NULL::bool THEN NULL::int4 ELSE p END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql  AS $$ SELECT CASE WHEN (md5(NULL::text) IS JSON ARRAY) THEN (GREATEST(p, p))::text::int4 ELSE (make_date(2024, 7, 1) - '2024-04-05'::date) END $$;

ANALYZE t2;

UPDATE t2 x SET b = x.b WHERE COALESCE((SELECT COALESCE('Z', 'b') FROM (SELECT 'a%' AS c0, quote_literal(to_char(NULL::numeric, 'FM999.00')) AS c1 FROM t1 t2 JOIN t2 t3 USING (b)) s1 WHERE s1.c0 >= s1.c1 COLLATE "und-x-icu" ORDER BY 1 LIMIT 1), concat('a', 3)) <= ' x' RETURNING x.*;

DELETE FROM t0 x USING t1 u WHERE x.id = u.id;

UPDATE t1 x SET b = x.a FROM (SELECT DISTINCT ON (id) id, b AS v FROM t1 ORDER BY id, 2) s WHERE s.id = x.id RETURNING NULLIF(true, COALESCE(false, false));

WITH d AS (DELETE FROM t2 x WHERE false RETURNING 0, 2) SELECT count(*) FROM d;

-- statement
INSERT INTO t0 AS x (id, a, b, c) SELECT sign(CASE ascii('z') WHEN 4 THEN (10.00::numeric)::int4 WHEN 0 THEN (s1.c1)::text::int4 END)::int, to_jsonb('2000-11-13'::date) AS c1, s1.c2, abs((abs(9::int8) * NULL::int4)) AS c3 FROM ((SELECT array_agg('bar' ORDER BY 'bar') FILTER (WHERE '1999-12-17'::date > '1999-04-19'::date) AS c0, (count(*))::int AS c1, min('a%') AS c2, min(date_trunc('day', ('2024-10-08 21:59:00'::timestamp AT TIME ZONE 'UTC'))) AS c3 FROM t1 t2 WHERE (- 3.75::float8) IS DISTINCT FROM abs((- -0::float8)) GROUP BY t2.a, (4)) s1 JOIN (VALUES ('2024-02-20'::date, false, make_date(2024, 2, 1)), ('2025-04-11'::date, true, '2000-04-01'::date)) s3(v0, v1, v2) ON starts_with(NULL::text, s1.c2)) RETURNING x.*;
