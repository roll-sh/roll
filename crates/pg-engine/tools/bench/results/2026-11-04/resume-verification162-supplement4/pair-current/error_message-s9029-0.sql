-- class: error_message
-- ordered: false
-- note: subquery uses ungrouped column "t2.c" from outer query | subquery uses ungrouped column "t1.e" from outer query
-- engine: ERROR 42803: subquery uses ungrouped column "t2.c" from outer query
-- engine: POSITION: 80
-- reference: ERROR 42803: subquery uses ungrouped column "t1.e" from outer query
-- reference: POSITION: 56

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (a interval, b text COLLATE "en-x-icu", c json);

INSERT INTO t0 (a, b, c) VALUES ('0', 'b', '1'), ('1 mon', 'Ä', 'null'), (NULL, 'ba', '{"a": 3}'), ('-1 day', 'a', NULL), ('0', 'a%', '[1, 2, 3]'), ('24:00:00', 'ba', '{"a": "1"}'), (NULL, '', '{"b": [1, "a"]}'), (NULL, 'abc', '"s"'), ('2 hours', NULL, '{"b": [1, "a"]}'), ('2 hours', 'Ab', NULL), ('24:00:00', NULL, '"s"'), ('2 hours', 'aa', NULL);

CREATE INDEX t0_i73 ON t0 USING hash (a);

CREATE INDEX t0_i357 ON t0 (a);

CREATE TABLE t1 (id int4 PRIMARY KEY, a d3text, b timestamp);

INSERT INTO t1 (id, a, b) VALUES (2, NULL, '2024-01-06 00:00:00'), (40, NULL, '2024-01-06 00:30:00'), (20, '10', '2024-01-05 12:30:00'), (24, 'x ', '2024-01-01 23:30:00'), (36, 'abc', '2024-01-04 00:00:00'), (25, 'ß', '2024-01-06 00:00:00'), (30, NULL, '2024-01-01 23:30:00'), (21, NULL, '2024-01-04 00:00:00'), (31, 'ab', '2024-01-08 00:30:00'), (12, NULL, '2024-01-02 12:00:00'), (35, '', '2024-01-01 00:30:00'), (49, 'é', '2024-01-06 06:30:00'), (38, 'z', '2024-01-05 06:00:00'), (37, NULL, '2024-01-02 00:00:00'), (27, 'foo', '2024-01-05 06:00:00'), (10, 'z', NULL), (13, 'ab', '2024-01-02 06:30:00'), (1, 'x ', '2024-01-07 12:00:00'), (18, 'a', '2024-01-04 12:30:00'), (33, 'Z', '2024-01-01 23:30:00'), (22, '', '2024-01-05 00:00:00'), (51, ' x', '2024-01-02 00:30:00'), (6, ' x', '2024-01-01 12:30:00'), (29, ' x', '2024-01-07 23:30:00'), (23, '9', '2024-01-02 00:00:00'), (7, 'ba', NULL), (59, 'ab', '2024-01-01 12:30:00'), (46, 'a', '2024-01-01 23:30:00'), (47, 'ß', '2024-01-03 06:30:00'), (9, 'b', '2024-01-08 06:00:00');

CREATE TABLE t2 (id int4 PRIMARY KEY, a d3int DEFAULT 4, b interval, c bool, d d3int, e int4);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (37, 2, '2 hours', NULL, -2, NULL), (38, NULL, '0', NULL, 8, NULL), (33, NULL, '1 mon', NULL, NULL, NULL);

CREATE INDEX t2_i936 ON t2 (d);

CREATE TABLE t3 (id int4, a numeric(10,2), b interval, c text, d int2, e int4[], g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL) PARTITION BY LIST (c);

CREATE TABLE t3_p0 PARTITION OF t3 FOR VALUES IN ('a', 'b', 'A');

CREATE TABLE t3_pd PARTITION OF t3 DEFAULT;

CREATE INDEX t3_i907 ON t3 USING hash (e);

CREATE INDEX t3_i724 ON t3 USING hash (id);

CREATE INDEX t3_i456 ON t3 (c) WHERE c IS NOT NULL;

CREATE TYPE ct3 AS (a text, b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

CREATE INDEX tc3_c ON tc3 (c);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF ('2025-01-07 03:00:00'::timestamp)::date < '-infinity'::date THEN
    r := ascii(q);
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(p, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT min(concat_ws('-', regexp_substr('b', '[^,]+', 1, 1), LEAST('ß', '10'))) AS c0, '{NULL,3}'::int4[] AS c1, max('2024-09-06'::date) AS c2 FROM (SELECT 'NaN'::numeric AS c0, array_ndims('{{NULL,1}}'::int4[]) AS c1, 1e3::numeric AS c2, sign(f0((t2.id & 8), quote_literal(t2.a)))::int AS c3 FROM t1 t2) s1 WHERE 'A' SIMILAR TO 'A%' GROUP BY s1.c2;

-- statement
CREATE VIEW v1 AS SELECT 'NaN'::numeric AS c0, (SELECT t1.e FROM t1 t3 WHERE ((t2.c) IS FALSE AND t2.c) ORDER BY 1 LIMIT 1) AS c1, min((SELECT GREATEST(t1.d, t4.d) FROM t3 t4 WHERE t2.c ORDER BY 1 LIMIT 1)) AS c2 FROM (t3 t1 JOIN t2 t2 ON t1.g = t2.d) WHERE isfinite('2025-09-19'::date) GROUP BY t2.a, t2.e;
