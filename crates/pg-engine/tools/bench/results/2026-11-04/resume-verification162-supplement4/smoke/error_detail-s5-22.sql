-- class: error_detail
-- ordered: true
-- note: 42601
-- engine: ERROR 42601: syntax error at or near ","
-- engine: POSITION: 925
-- reference: ERROR 42601: syntax error at or near ","
-- reference: POSITION: 926

CREATE TABLE t0 (id int4 PRIMARY KEY, a timestamp, b int4[]);

INSERT INTO t0 (id, a, b) VALUES (37, '2024-01-03 00:30:00', '{}');

CREATE INDEX t0_i644 ON t0 (id) WHERE id IS NOT NULL;

CREATE INDEX t0_i14 ON t0 USING hash (id);

CREATE INDEX t0_i221 ON t0 (a, id);

CREATE TABLE t1 (id int4, a float8, b int2);

INSERT INTO t1 (id, a, b) VALUES (1, 3, 2), (19, -1, 3), (28, -1.5, 2), (21, -1.25, 2), (6, -1, 5), (38, 3.75, 5), (23, 3, 5), (14, -1.75, 2), (31, -1, 6), (54, 0.5, 5), (34, 3.5, 2), (35, 1.5, 6), (32, NULL, 4), (47, 1.5, 4), (37, 3.5, 3), (18, NULL, 1), (13, 3, NULL), (53, NULL, 0), (36, 1.75, 1), (20, NULL, 1);

CREATE INDEX t1_i451 ON t1 (a);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN COALESCE('1'::jsonb ? 'k', true) THEN (('[1, 2, 3]'::jsonb || 'null'::jsonb)::jsonb ->> 'a')::int ELSE array_length('{NULL,3}'::int4[], 1) END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE sql STABLE AS $$ SELECT id FROM t1 WHERE id <= lim $$;

UPDATE t1 x SET b = x.b WHERE CASE WHEN NOT (false) THEN to_jsonb('{1,1,2}'::int4[]) ELSE to_jsonb(x.id) END <> 'null'::jsonb RETURNING x.*;

INSERT INTO t1 AS x (id, a, b) VALUES (2, 2.25, 4) RETURNING x.*;

-- statement
SELECT DISTINCT ON (1) ceil(NULLIF(('NaN'::numeric)::numeric(6,1), 'NaN'::numeric)) AS c0, (SELECT array_agg(initcap('aa') ORDER BY initcap('aa')) FILTER (WHERE CASE WHEN NULL::bool THEN true ELSE true END) FROM (SELECT CASE WHEN true THEN (- sign(t8.id)::int) ELSE t8.id END AS c0, abs((abs(3.75::float8) + LEAST(t8.a, t8.a))) AS c1, trim(replace(NULL::text, 'a', 'x')) AS c2, ('2024-10-26 19:59:00'::timestamp - (t6.a - t5.a)) AS c3 FROM t1 t8 WHERE true) s7) AS c1, (abs(abs(0::int8)) + abs(6::int8)) AS c2 FROM (((SELECT ascii(' x') AS c0, ((lpad('ß', 3, '*') COLLATE "POSIX") || upper(quote_literal('9'))) AS c1 FROM t0 t2 RIGHT JOIN t1 t3 USING (id) WHERE ('ab' NOT BETWEEN regexp_replace('9', '[ab]', 'y', 'g') AND upper('x ')) IS NOT UNKNOWN) INTERSECT ALL (SELECT DISTINCT extract(day from ('2024-11-07 09:00:00'::timestamp)::date)::int AS c0, COALESCE(upper(md5('x ')), COALESCE('a%', 'a_b')) AS c1 FROM t1 t4)) s1 , t0 t5 LEFT JOIN t0 t6 ON t5.a = t6.a) ORDER BY 1 DESC, 2 DESC NULLS LAST, 3;
