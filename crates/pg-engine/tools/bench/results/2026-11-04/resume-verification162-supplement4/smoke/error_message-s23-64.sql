-- class: error_message
-- ordered: true
-- note: subquery uses ungrouped column "s1.c1" from outer query | column "s1.c0" must appear in the GROUP BY clause or be used in an aggregate function
-- engine: ERROR 42803: subquery uses ungrouped column "s1.c1" from outer query
-- engine: POSITION: 337
-- reference: ERROR 42803: column "s1.c0" must appear in the GROUP BY clause or be used in an aggregate function
-- reference: POSITION: 115

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b int4, c timestamptz, d date);

CREATE TABLE t1 (id int4 PRIMARY KEY, a int4, b int4, c text COLLATE "en-x-icu", d timestamp);

INSERT INTO t1 (id, a, b, c, d) VALUES (34, 1, 3, 'ab', NULL), (36, 7, 9, 'ß', '2024-01-08 23:30:00'), (11, NULL, 3, '9', '2024-01-09 12:30:00'), (38, 4, 5, 'ß', '2024-01-08 00:00:00'), (28, -1, 5, 'Z', '2024-01-07 23:30:00'), (24, 5, 4, 'a', '2024-01-04 23:30:00'), (5, 6, 4, 'A', '2024-01-06 00:30:00'), (6, 4, NULL, 'z', '2024-01-04 12:30:00'), (8, 8, -1, 'abc', '2024-01-08 06:00:00'), (7, 9, 5, ' x', '2024-01-03 23:00:00'), (35, NULL, 0, 'a,b', '2024-01-04 23:30:00'), (1, 3, 0, 'a', NULL), (15, 5, NULL, 'a', '2024-01-09 06:30:00'), (20, 1, -1, 'é', NULL), (40, NULL, 1, 'Z', '2024-01-08 00:30:00'), (57, 2, NULL, 'ab', '2024-01-04 06:00:00'), (2, 2, 0, 'B', '2024-01-02 12:00:00'), (29, NULL, 3, 'abc', '2024-01-06 06:30:00'), (4, 7, 7, '', '2024-01-01 00:00:00'), (51, NULL, 7, 'foo', NULL);

CREATE TABLE t2 (id int4 PRIMARY KEY, a json, b text[], c int4);

CREATE INDEX t2_i802 ON t2 (c);

ANALYZE;

CREATE VIEW v0 AS SELECT 9::int2 AS c0, ('aa' || 'ab') AS c1 FROM t0 t1;

CREATE VIEW v1 AS SELECT -0::float8 AS c0, (t1.c)::int2 AS c1, abs((GREATEST(8::int8, 7::int8) + 7::int8)) AS c2 FROM t2 t1 WHERE (SELECT bool_or(t1.b > '{a,b}'::text[]) FROM t0 t2 WHERE true);

DO $$ BEGIN RAISE NOTICE 'v=%', 'a_b'; END $$;

INSERT INTO t2 AS x (id, a, b, c) VALUES (20, '{"b": [1, "a"]}', '{a,b}', -1);

INSERT INTO t0 AS x (id, a, b, c, d) SELECT 10, 8, sign(abs(0))::int, date_trunc('month', ((SELECT max(('2000-08-26 22:00:00'::timestamp AT TIME ZONE 'UTC')) FROM t1 t3 WHERE NULL::bool) + '-1 mon'::interval)), ('2024-11-11 05:59:00'::timestamp)::date FROM (v0 t1 JOIN v0 t2 ON t1.c0 = t2.c0) ORDER BY 1 NULLS FIRST, 2 DESC NULLS LAST, 3, 4 DESC, 5 LIMIT 5 ON CONFLICT DO NOTHING;

UPDATE t0 x SET d = x.d WHERE NULL::bool RETURNING LEAST((x.id)::numeric, round(10.00::numeric, 1)), (GREATEST(-7.125::numeric, 1::numeric))::numeric(6,1);

MERGE INTO t1 x USING (SELECT id, c FROM t1 WHERE id % 3 = 0) s ON s.id = x.id WHEN MATCHED AND false THEN UPDATE SET a = x.b WHEN MATCHED AND true THEN DELETE;

ANALYZE t1;

UPDATE t2 x SET c = (('{1,2}'::int4[] || x.id))[1];

INSERT INTO t1 AS x (id, a, b, c, d) VALUES (7, -1, 9, DEFAULT, '2024-01-08 00:30:00'), (60, DEFAULT, 0, 'é', NULL), (47, 9, 6, '10', '2024-01-03 00:30:00') ON CONFLICT (id) DO UPDATE SET a = LEAST(x.id, ('NaN'::numeric)::int4);

MERGE INTO t2 x USING (SELECT id FROM t1 WHERE id % 2 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET b = x.b WHEN MATCHED AND '{}'::jsonb ? 'k' THEN DELETE WHEN NOT MATCHED BY SOURCE THEN DELETE RETURNING merge_action(), x.*;

INSERT INTO t1 AS x (id, a, b, c, d) VALUES (35, DEFAULT, NULL, 'abc', '2024-01-07 06:30:00'), (14, 1, 7, DEFAULT, '2024-01-09 00:00:00'), (15, 5, DEFAULT, NULL, DEFAULT) ON CONFLICT (id) DO UPDATE SET b = ((x.a - x.b) / 3);

UPDATE t0 x SET c = date_trunc('day', date_trunc('month', s.v)) FROM (SELECT DISTINCT ON (id) id, c AS v FROM t0 ORDER BY id, 2) s WHERE s.id = x.id RETURNING ('{"a": 1, "b": "x"}'::jsonb::jsonb ->> 'a')::int;

UPDATE t2 x SET c = LEAST((x.c)::text::int4, (x.c - x.id)) FROM (SELECT DISTINCT ON (id) id, c AS v FROM v0 ORDER BY id, 2) s WHERE s.id = x.id RETURNING (SELECT (count(*))::int FROM (SELECT '{}'::int4[] AS c0 FROM (t1 t2 JOIN t2 t3 ON t2.b = t3.id)) s1);

MERGE INTO t2 x USING (SELECT id, c FROM t2 WHERE id % 3 = 0) s ON s.id = x.id WHEN MATCHED AND 1::numeric BETWEEN 12345678.9::numeric AND 'NaN'::numeric THEN UPDATE SET c = (x.c % 2) RETURNING merge_action(), x.*;

UPDATE t2 x SET b = x.b FROM (SELECT DISTINCT ON (id) id, b AS v FROM t2 ORDER BY id, 2) s WHERE s.id = x.id RETURNING x.*;

MERGE INTO t0 x USING (SELECT id, b FROM t1 WHERE id % 2 = 0) s ON s.id = x.id WHEN MATCHED AND '1 mon -1 day'::interval <= '1 mon -1 day'::interval THEN UPDATE SET c = x.c WHEN NOT MATCHED THEN INSERT (id, a, b, c, d) VALUES (s.id + 100, DEFAULT, DEFAULT, NULL, DEFAULT);

UPDATE t1 x SET a = x.id FROM (SELECT DISTINCT ON (id) id, a AS v FROM t0 ORDER BY id, 2) s WHERE s.id = x.id;

UPDATE t2 x SET b = array_append(array_append(s.v, 'a,b'), replace('Ab', 'a', 'x')) FROM (SELECT DISTINCT ON (id) id, b AS v FROM v0 ORDER BY id, 2) s WHERE s.id = x.id RETURNING x.*;

-- statement
SELECT DISTINCT CASE WHEN ('{1,2}'::int4[])[1:2] IS NOT DISTINCT FROM '{1,2}'::int4[] THEN ('1999-04-18'::date - (s1.c0 & NULL::int4)) ELSE '2024-07-21'::date END AS c0, (SELECT max(date_trunc('day', s4.c0)) FROM (SELECT min(t6.d) AS c0, every(isfinite(make_date(2024, 11, 1))) FILTER (WHERE s1.c1) AS c1 FROM (t2 t5 LEFT JOIN t1 t6 ON s1.c1) WHERE ((array_position('{5}'::int4[], t5.c) / 1), initcap(t6.c)) = ((t6.id)::text::int4, t6.c) GROUP BY t5.b HAVING bool_and(t6.id <= -3::int2)) s4) AS c1 FROM (SELECT (count(*))::int AS c0, string_to_array('10', ',') <@ t2.b AS c1 FROM t2 t2 JOIN t2 t3 USING (id) GROUP BY t2.b HAVING every(false)) s1 WHERE '2000-09-24 19:00:00+00'::timestamptz > (('2000-02-22 06:59:00'::timestamp + '1 year 2 days'::interval) AT TIME ZONE 'UTC') ORDER BY 1, 2;
