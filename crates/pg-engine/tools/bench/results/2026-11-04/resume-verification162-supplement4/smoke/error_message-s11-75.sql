-- class: error_message
-- ordered: true
-- note: subquery uses ungrouped column "s1.c2" from outer query | subquery uses ungrouped column "t10.c" from outer query
-- engine: ERROR 42803: subquery uses ungrouped column "s1.c2" from outer query
-- engine: POSITION: 1348
-- reference: ERROR 42803: subquery uses ungrouped column "t10.c" from outer query
-- reference: POSITION: 1365

CREATE TABLE t0 (id int4, a json, b int4 DEFAULT 1, c bool, d int8, e text) PARTITION BY RANGE (b);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t0_p1 PARTITION OF t0 FOR VALUES FROM (3) TO (6);

CREATE TABLE t0_p2 PARTITION OF t0 FOR VALUES FROM (6) TO (MAXVALUE);

CREATE INDEX t0_i327 ON t0 (e) WHERE e IS NOT NULL;

CREATE INDEX t0_i962 ON t0 (e);

CREATE INDEX t0_i107 ON t0 (e) WHERE e IS NOT NULL;

CREATE TABLE t1 (a text, b timestamptz);

INSERT INTO t1 (a, b) VALUES ('aa', '2024-01-01 12:00:00+00'), ('a%', '2024-01-08 23:00:00+00'), ('ba', '2024-01-07 23:00:00+00'), ('ab', '2024-01-09 12:00:00+00'), ('ab', '2024-01-04 06:00:00+00'), ('B', '2024-01-02 12:00:00+00'), ('b', '2024-01-01 00:00:00+00'), (NULL, '2024-01-05 06:00:00+00'), ('é', '2024-01-08 00:00:00+00'), ('Ä', '2024-01-04 12:00:00+00'), ('a,b', '2024-01-08 06:00:00+00'), ('bar', '2024-01-02 00:00:00+00'), ('a', '2024-01-02 23:00:00+00'), ('ba', NULL), ('b', '2024-01-09 12:00:00+00'), ('b', '2024-01-04 00:00:00+00'), ('a,b', '2024-01-04 23:00:00+00'), ('ß', '2024-01-06 23:00:00+00'), ('bar', '2024-01-01 23:00:00+00'), ('9', '2024-01-03 23:00:00+00'), ('bar', '2024-01-02 06:00:00+00'), ('a,b', NULL), (NULL, NULL), ('a,b', '2024-01-07 23:00:00+00'), ('bar', '2024-01-02 12:00:00+00'), ('abc', '2024-01-05 12:00:00+00'), ('ba', '2024-01-02 00:00:00+00'), ('ba', '2024-01-08 23:00:00+00'), ('Ab', '2024-01-03 12:00:00+00'), ('é', '2024-01-07 23:00:00+00');

CREATE INDEX t1_i333 ON t1 (b DESC NULLS LAST);

CREATE INDEX t1_i72 ON t1 (b) WHERE b IS NOT NULL;

CREATE TABLE t2 (id int4 PRIMARY KEY, a timestamptz, b int2, c int8);

INSERT INTO t2 (id, a, b, c) VALUES (23, '2024-01-02 00:00:00+00', 4, 2), (33, NULL, 4, 2), (35, '2024-01-06 12:00:00+00', 0, 12);

CREATE INDEX t2_i213 ON t2 (c);

CREATE INDEX t2_i213 ON t2 (c) WHERE c IS NOT NULL;

CREATE INDEX t2_i157 ON t2 USING hash (a);

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE sql STABLE AS $$ SELECT id, e FROM t0 WHERE id <= lim $$;

INSERT INTO t1 AS x (a, b) SELECT concat_ws('-', 'a%', lower('abc')) AS c0, t1.a AS c1 FROM t2 t1 WHERE false RETURNING CASE WHEN 3.75::float8 <= 2::float8 THEN CASE 1 WHEN 3 THEN 3 WHEN 3 THEN 8 END ELSE width_bucket(1e3::numeric, 0, 10, 4) END, mod(('NaN'::numeric * -7.125::numeric), 3);

INSERT INTO t1 AS x (a, b) VALUES ('aa', DEFAULT), ('b', NULL);

ANALYZE t2;

DELETE FROM t0 x WHERE abs(abs(3::int8)) IS NOT DISTINCT FROM CASE WHEN starts_with(x.e, x.e) THEN abs(NULL::int8) ELSE (x.d * x.b) END RETURNING (jsonb_strip_nulls(NULL::jsonb) ->> 'a'), x.e;

MERGE INTO t2 x USING (SELECT id, b FROM t2 WHERE id % 2 = 0) s ON s.id = x.id WHEN MATCHED THEN UPDATE SET a = ((x.a + '1 year 2 days'::interval) + justify_hours('1 year 2 days'::interval)) WHEN NOT MATCHED THEN INSERT (id, a, b, c) VALUES (s.id + 100, '2024-01-07 12:00:00+00', 5, 10);

INSERT INTO t2 AS x (id, a, b, c) SELECT (count(*) FILTER (WHERE (t1.c OR t1.c)))::int, min('2024-05-02 11:00:00+02:30'::timestamptz), min(12::int2), count(DISTINCT width_bucket((NULL::numeric / 3), 0, 10, 4)) FROM t0 t1 GROUP BY t1.c, t1.id;

-- statement
SELECT sum(t10.id) AS c0, min(split_part(COALESCE(t10.e, t10.e), ',', 1)) AS c1, sum((s1.c1 * t10.b)) AS c2 FROM ((WITH w2 AS (SELECT round(trunc((NULL::numeric - 12)), 2) AS c0, NULLIF((- t3.id), t3.id) AS c1 FROM t2 t3 WHERE false), w4 AS NOT MATERIALIZED (SELECT (- (- 2::float8)) AS c0, LEAST('2 hours'::interval, (('1 mon -1 day'::interval * t5.b) * t6.c1)) AS c1, t5.e AS c2, (CASE (t5.b & t5.b) WHEN 0 THEN CASE WHEN t5.c THEN 0 ELSE t6.c1 END WHEN 1 THEN num_nonnulls((-3), t5.e) END + t5.b) AS c3 FROM (t0 t5 JOIN w2 t6 ON t5.id = t6.c1) WHERE string_to_array(lower(t5.e), ',') IS NOT DISTINCT FROM array_append(ARRAY[t5.e, t5.e], split_part(t5.e, ',', 1)) ORDER BY 1, 2, 3, 4) SELECT (('2024-01-03'::date - (s8.u % 1)) - (SELECT '2025-08-16'::date FROM t0 t9 WHERE NOT (t9.c) ORDER BY 1 LIMIT 1)) AS c0, s8.o AS c1, false AS c2 FROM (t1 t7 JOIN unnest('{}'::int4[]) WITH ORDINALITY s8(u, o) ON 'null'::jsonb ? 'k') WHERE (s8.u, t7.a) > ((- cardinality('{}'::int4[])), 'a')) s1 LEFT JOIN t0 t10 ON s1.c0 = t10.id AND s1.c0 < t10.b) HAVING ('[]'::jsonb ->> 'b') = ALL (SELECT max(s11.c0) AS c0 FROM (SELECT DISTINCT t12.a AS c0, (((s1.c0)::int8 * array_length('{5}'::int4[], 1)) + s1.c1) AS c1, CASE WHEN false THEN length(COALESCE(t12.a, t12.a)) ELSE COALESCE((- t10.id), position(t12.a in 'ab')) END AS c2, t12.b AS c3 FROM t1 t12 WHERE s1.c2) s11 WHERE t10.c GROUP BY ROLLUP (s11.c3, s11.c1) HAVING s1.c2 ORDER BY 1) ORDER BY 1 NULLS FIRST, 2 DESC NULLS LAST, 3;
