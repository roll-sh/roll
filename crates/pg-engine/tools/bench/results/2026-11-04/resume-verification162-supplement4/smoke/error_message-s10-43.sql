-- class: error_message
-- ordered: false
-- note: subquery uses ungrouped column "t1.b" from outer query | subquery uses ungrouped column "t1.a" from outer query
-- engine: ERROR 42803: subquery uses ungrouped column "t1.b" from outer query
-- engine: POSITION: 184
-- reference: ERROR 42803: subquery uses ungrouped column "t1.a" from outer query
-- reference: POSITION: 146

CREATE TABLE t0 (id int4 PRIMARY KEY, a timestamp, b timestamptz, c int4, d int4 DEFAULT 4, e jsonb);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (24, '2024-01-07 12:00:00', '2024-01-05 00:00:00+00', -1, NULL, '[1, 2, 3]'), (3, '2024-01-04 12:00:00', '2024-01-02 12:00:00+00', 4, 0, 'null'), (29, '2024-01-08 06:00:00', NULL, NULL, 7, NULL), (39, '2024-01-08 00:30:00', NULL, NULL, 6, '{"a": {"b": 2}}'), (33, '2024-01-05 12:00:00', '2024-01-08 12:00:00+00', NULL, -2, '{"a": 1, "b": "x"}'), (22, NULL, '2024-01-03 23:00:00+00', 4, -2, NULL), (23, NULL, '2024-01-05 23:00:00+00', 1, 5, '[1, 2, 3]'), (11, NULL, '2024-01-01 00:00:00+00', 7, 8, '{"a": 1, "b": "x"}'), (35, '2024-01-05 12:30:00', '2024-01-09 23:00:00+00', 0, -1, NULL), (4, '2024-01-04 00:00:00', '2024-01-05 06:00:00+00', NULL, 5, '1'), (30, '2024-01-06 23:00:00', '2024-01-07 00:00:00+00', 4, 1, 'null'), (58, '2024-01-04 00:30:00', NULL, 3, 1, '1');

CREATE INDEX t0_i996 ON t0 (b) WHERE b IS NOT NULL;

CREATE TABLE t1 (id int4, a numeric(10,2), b text COLLATE "und-x-icu", c date, d timestamptz) PARTITION BY RANGE (b);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES FROM (MINVALUE) TO ('b');

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

INSERT INTO t1 (id, a, b, c, d) VALUES (40, 5.00, 'abc', '2024-01-26', '2024-01-08 23:00:00+00'), (28, 7.05, 'aa', '2024-03-11', '2024-01-04 06:00:00+00'), (7, 0.25, 'a%', '2024-02-08', '2024-01-06 23:00:00+00'), (38, 11.75, '9', '2024-03-03', '2024-01-08 12:00:00+00'), (26, 4.50, '', '2024-01-22', '2024-01-08 23:00:00+00'), (20, NULL, '', NULL, '2024-01-01 00:00:00+00'), (15, -1.25, '9', '2024-02-26', '2024-01-03 12:00:00+00'), (36, -3.00, 'Ä', '2024-01-17', NULL);

CREATE INDEX t1_i295 ON t1 (c);

CREATE INDEX t1_i669 ON t1 (id, a);

CREATE INDEX t1_i898 ON t1 (a);

CREATE TABLE t2 (id int4, a int4[], b text) PARTITION BY RANGE (b);

CREATE TABLE t2_p0 PARTITION OF t2 FOR VALUES FROM (MINVALUE) TO ('b');

CREATE TABLE t2_pd PARTITION OF t2 DEFAULT;

INSERT INTO t2 (id, a, b) VALUES (3, '{1,2}', ''), (8, NULL, 'ba'), (36, '{2}', 'x '), (23, '{NULL,3}', 'a%'), (22, '{5}', 'A'), (27, '{1,2}', 'Z'), (24, '{1,1,2}', 'foo'), (31, NULL, 'B'), (29, '{5}', 'z'), (7, NULL, NULL), (39, '{NULL,3}', 'foo'), (40, '{2}', 'é'), (53, '{5}', 'a,b'), (25, NULL, ''), (26, '{5}', 'z'), (41, NULL, 'x '), (18, '{3,1,2}', 'aa'), (15, '{}', 'aa'), (1, NULL, 'b'), (2, '{NULL,3}', 'bar');

CREATE TABLE t3 (id int4, a int4, b int4, c int4);

CREATE TABLE t3_c (extra int) INHERITS (t3);

INSERT INTO t3_c (id, a, b, c, extra) VALUES (521, 2, 5, 7, 3);

INSERT INTO t3_c (id, a, b, c, extra) VALUES (536, 0, 6, 6, 1);

INSERT INTO t3_c (id, a, b, c, extra) VALUES (631, NULL, 1, 5, 0);

INSERT INTO t3 (id, a, b, c) VALUES (24, 1, NULL, -1), (19, -2, 5, 3), (34, -2, 2, 5), (40, -2, 3, -2), (31, -1, 6, NULL), (37, 6, 4, -2), (1, 1, 3, -2), (30, -2, 3, 8), (47, 3, 3, 5), (15, 6, -2, -2), (13, NULL, 3, NULL), (60, 4, -1, NULL), (39, -1, 5, 4), (18, NULL, 9, 7), (38, -1, NULL, 2), (27, 9, -2, 2), (46, 2, 5, 7), (11, 3, -2, 2), (35, 6, NULL, NULL), (42, -2, NULL, 5), (2, 1, 5, 6), (7, NULL, NULL, 4), (10, 8, 5, 8), (16, 0, 3, NULL), (17, -2, 8, 2), (33, -1, -1, NULL), (50, 2, 4, NULL), (28, 3, 0, 2), (5, 4, 3, 7), (14, 1, -1, NULL);

CREATE INDEX t3_i297 ON t3 (b);

CREATE INDEX t3_i243 ON t3 (id);

ANALYZE;

CREATE VIEW v0 AS SELECT (SELECT bool_and(false) FILTER (WHERE false) FROM t0 t3 WHERE NULLIF(false, false)) AS c0, CASE array_length((t1.a || t1.a), 1) WHEN 5 THEN jsonb_array_length(jsonb_build_object('k', t2.c, 'v', t1.b)) WHEN 4 THEN t2.a END AS c1, (trunc(abs(NULL::numeric)) + mod(NULL::numeric, 3)) AS c2 FROM (t2 t1 LEFT JOIN t3 t2 ON t1.id = t2.id) WHERE t2.c <= jsonb_array_length('"s"'::jsonb);

CREATE VIEW v1 AS SELECT 'abc' AS c0, (('2024-08-10'::date + t2.c))::timestamp AS c1, 99.99::numeric AS c2, count(CASE WHEN 'a_b' > 'a_b' THEN (0::float8)::text::float8 ELSE 0.5::float8 END) AS c3 FROM t0 t2 WHERE regexp_replace(NULLIF('Ä', 'z'), '[ab]', 'y', 'g') <> 'a_b' GROUP BY t2.c;

DO $$ BEGIN RAISE NOTICE 'v=%', concat(regexp_replace(NULL::text, '[ab]', 'y', 'g'), ('1999-09-15'::date - NULL::date)); END $$;

DELETE FROM t1 x USING v0 u WHERE x.id = u.c1 AND isfinite(make_date(2024, 9, 1)) RETURNING (('{1,2}'::int4[])[1] * GREATEST(x.id, x.id));

ANALYZE t0;

INSERT INTO t1 AS x (id, a, b, c, d) VALUES (19, NULL, DEFAULT, DEFAULT, '2024-01-01 23:00:00+00') RETURNING NOT (NULL::bool);

ANALYZE t1;

INSERT INTO t1 AS x (id, a, b, c, d) VALUES (58, DEFAULT, 'A', DEFAULT, '2024-01-04 23:00:00+00');

-- statement
SELECT (SELECT count(COALESCE(s2.c0, s2.c0)) FROM ((SELECT CASE WHEN ((NULL::bool AND false) AND t3.e @> t3.e) THEN ((t3.a)::date - width_bucket(t1.a, 0, 10, 4)) ELSE t1.c END AS c0, t1.b AS c1 FROM t0 t3 CROSS JOIN t0 t4 WHERE (ascii(t1.b) / 3) > ALL ((ARRAY[6, t4.id] || position(t1.b in 'ß')))) INTERSECT (SELECT max(t1.c) FILTER (WHERE true) AS c0, string_agg(split_part(quote_literal(t1.b), ',', 1), ',' ORDER BY split_part(quote_literal(t1.b), ',', 1) DESC) AS c1 FROM t0 t5 GROUP BY GROUPING SETS ((t5.c), ()))) s2 WHERE s2.c1 = reverse(s2.c1) COLLATE "C") AS c0, (5::int2 + LEAST(-2::int2, NULL::int2)) AS c1 FROM t1 t1 WHERE true GROUP BY t1.d, (t1.id);
