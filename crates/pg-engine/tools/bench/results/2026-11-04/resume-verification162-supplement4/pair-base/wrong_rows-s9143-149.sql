-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a99924  (rows=13)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a99924  (rows=856)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (a int8, b int4[], c timestamp, d text[], e timestamptz);

INSERT INTO t0 (a, b, c, d, e) VALUES (NULL, NULL, NULL, '{}', NULL), (NULL, '{1,2}', NULL, NULL, NULL), (NULL, '{3,1,2}', '2024-01-05 00:30:00', NULL, NULL), (NULL, '{}', '2024-01-06 06:00:00', NULL, NULL), (0, NULL, NULL, NULL, NULL), (NULL, '{}', NULL, NULL, NULL), (NULL, '{NULL,3}', NULL, '{b,NULL}', NULL), (NULL, '{3,1,2}', NULL, '{z}', NULL);

CREATE INDEX t0_i865 ON t0 (d);

CREATE TABLE t1 (id int4 PRIMARY KEY, a int4, b float8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t1 (id, a, b) VALUES (34, NULL, NULL), (39, -1, 1), (14, 7, NULL), (25, 4, NULL), (27, 2, -1.5), (13, NULL, 2.75), (7, -1, 0.75), (35, NULL, NULL), (8, -2, NULL), (1, 3, -2), (4, NULL, NULL), (23, NULL, 1);

CREATE TABLE t2 (id int4 PRIMARY KEY, a text, b int2, c numeric(10,2), d interval, e interval);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (34, NULL, NULL, NULL, '0', '1 day'), (36, 'x ', 1, 4.00, NULL, '-1 day'), (7, 'A', NULL, NULL, NULL, '2 hours');

CREATE INDEX t2_i539 ON t2 (e) WHERE e IS NOT NULL;

CREATE INDEX t2_i808 ON t2 (id);

CREATE INDEX t2_i430 ON t2 (e, b);

CREATE TABLE t3 (id int4 PRIMARY KEY, a text, b numeric(10,2), c text, d text COLLATE "en-x-icu", e text DEFAULT 'é');

INSERT INTO t3 (id, a, b, c, d, e) VALUES (27, 'bar', NULL, 'Z', 'z', 'é'), (23, 'a%', -3.75, ' x', 'aa', 'B'), (11, 'abc', 5.05, '9', '10', 'Ab'), (33, 'Z', 10.05, '', 'b', ' x'), (5, 'bar', 1.50, 'Ä', 'Ä', NULL), (36, 'x ', NULL, 'a_b', 'foo', 'foo'), (35, 'ab', 0.00, '', 'foo', 'bar'), (7, 'b', 10.25, NULL, 'ß', '9'), (9, 'a%', 0.25, NULL, 'foo', 'foo'), (29, 'a%', 9.00, '10', 'foo', 'a%'), (24, 'z', 9.50, 'A', 'Z', 'bar'), (4, 'aa', 12.50, 'bar', '10', 'é'), (54, 'ß', 1.00, 'aa', 'ab', 'abc'), (21, NULL, 7.50, 'ab', 'b', ''), (14, 'aa', NULL, 'é', 'A', 'abc'), (16, 'Z', 8.05, 'a', NULL, ' x'), (12, 'Z', 8.05, 'Z', 'B', 'é'), (6, 'ß', 6.75, 'ab', 'a%', 'A'), (15, 'A', 2.50, 'Ä', NULL, 'bar'), (30, 'a_b', -3.00, 'B', 'foo', 'z'), (28, 'foo', 3.50, NULL, 'é', ''), (40, 'Z', NULL, 'Z', NULL, 'x '), (25, '9', -3.50, 'ba', '', 'a,b'), (19, 'ba', -2.25, 'Ä', 'ß', 'bar'), (8, 'foo', 10.00, '10', NULL, NULL), (32, 'B', 3.25, ' x', 'x ', 'aa'), (51, 'B', 11.50, 'A', '', 'B'), (49, 'a,b', 10.50, 'A', 'A', 'Z'), (38, '', 8.00, 'a_b', NULL, 'ß'), (22, 'ab', -2.25, 'Ab', 'a,b', 'foo');

CREATE INDEX t3_i695 ON t3 USING hash (id);

CREATE TYPE ct3 AS (a text, b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('Z', 3)::ct3, NULL, 3), (2, ROW('Z', 1)::ct3, ARRAY[ROW('9', -1)::ct3, ROW('a_b', -1)::ct3], 3), (3, ROW('A', -1)::ct3, ARRAY[ROW(NULL, 0)::ct3, ROW('é', 1)::ct3], 1), (4, ROW('B', 1)::ct3, ARRAY[ROW(' x', 0)::ct3, ROW(NULL, 0)::ct3], 1), (5, ROW('é', 2)::ct3, ARRAY[ROW('é', 1)::ct3, ROW('9', 2)::ct3], 2), (6, NULL, ARRAY[ROW(' x', 3)::ct3, ROW('ß', 0)::ct3], 3), (7, ROW('é', 2)::ct3, NULL, 1), (8, ROW('ab', 2)::ct3, '{}', 2);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
DECLARE r int := 0;
BEGIN
  IF true THEN
    r := cardinality(array_remove('{1,1,2}'::int4[], p));
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(abs((10.00::numeric)::int4), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN q NOT IN (btrim(q, ' a')) THEN RETURN NULLIF(sign(p)::int, p);
  ELSE RETURN position(to_char(100::numeric, 'FM999.00') in concat_ws('-', q, q));
  END CASE;
END
$$;

CREATE VIEW v0 AS SELECT split_part((SELECT string_agg(t2.d, ',' ORDER BY t2.d) FROM t3 t2 WHERE COALESCE('[1, 2, 3]'::jsonb <> '{"b": [1, "a"]}'::jsonb, false)), ',', 1) AS c0, ((CASE WHEN false THEN t1.b ELSE t1.b END || 8))[1] AS c1, 9::int8 AS c2 FROM t0 t1 WHERE (10 + (2 * 9)) IS NOT DISTINCT FROM 8;

CREATE VIEW v1 AS SELECT (count(*))::int AS c0, GROUPING(t1.b, (t1.id)) FROM t3 t1 WHERE NOT EXISTS (SELECT DISTINCT CASE WHEN CASE WHEN true THEN t2.c ELSE t2.c END IS NULL THEN (COALESCE(NULL::int4, t1.id) + t1.id) ELSE (('NaN'::float8 * 0::float8))::int END FROM t0 t2 ORDER BY 1 LIMIT 4) GROUP BY ROLLUP (t1.b, (t1.id));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', NULL, '2025-03-09 02:30:00', '-infinity', '-infinity', '-1e-20', '-3', NULL), (2, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '1970-01-01 00:00:00.5', '2024-02-29', '-1 mon 2 days', 'Infinity', '0.125', NULL), (3, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2024-02-29 23:00:00+00', '2000-01-01 00:00:00', '1999-12-31', '1 year 2 mons', '-0.0', '0.5', 0), (4, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', NULL, '1970-01-01 00:00:00.5', '1999-12-31', '2 weeks 3 microseconds', '99999999999999999999', '-1.25', NULL), (5, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', 'infinity', '1999-12-31', '36 hours', '-1e-20', '-3', 0);

INSERT INTO tp4 VALUES (13, NULL, 'ab', '1e30'), (14, '4713-01-01 BC', NULL, '-0.0'), (18, '-infinity', 'c', NULL), (20, NULL, 'a', '0'), (10, '-infinity', 'b', '-1e-20'), (6, '-infinity', 'b', '2.5'), (12, '2024-02-29', 'c', '-2.5'), (19, '2025-03-09', 'a', '0.000001'), (25, '4713-01-01 BC', 'a', '-Infinity'), (22, '2025-01-01', 'ab', '-1e-20');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "tr-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 4, 'Z', '1e100', '-178000000 years', '[5,6)', '{empty}', 'the and of', ARRAY[ROW(NULL, 'c')::r5ct]), (2, 8, 'c', '-1e40', '720:00:00', '[3,)', '{[0,100)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(3, 'ss')::r5ct]), (3, NULL, 'ö', '-0.5', '2147483647 days', '[0,10)', '{[1,2],[2,3)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(3, 'ß')::r5ct, ROW(4, 'A')::r5ct, ROW(3, 'ch')::r5ct]), (4, 16, '', NULL, '-2147483648 days', '[3,)', '{(,0],[10,)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(NULL, 'o')::r5ct]), (5, NULL, '9', '123456789012345678901234567890.123456789', '0', '[3,)', '{empty}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(NULL, 'E')::r5ct, ROW(5, 'å')::r5ct, ROW(5, NULL)::r5ct]), (6, NULL, '-a', '7', '720:00:00', '(,2]', '{}', 'the and of', ARRAY[ROW(-1, 'e')::r5ct]), (7, 4, NULL, NULL, '-infinity', '[5,6)', '{[0,100)}', 'Fat cats! FAT rats? fat-free', NULL), (8, 2, 'AB', '1e-130', '178000000 years', '[-3,0]', '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', NULL);

CREATE INDEX r5a_b ON r5a (b COLLATE "r5ai");

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (NEW.g1 > 0) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 3 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 8 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (-4, 'a', 'infinity'), (10, 'b', '2025-01-01'), (2, 'c', NULL), (12, 'd', '2025-01-01'), (16, 'a', '2025-01-01'), (2, 'b', NULL), (-4, 'c', '2025-01-01'), (7, 'd', 'infinity'), (15, 'a', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL), (27, 'x1', NULL), (32, 'x2', NULL), (22, 'x3', NULL), (23, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 1, '{b,NULL}', NULL), (2, 'u2@x.org', 'dee2', 0, '{b,NULL}', NULL), (3, 'u3@x.org', 'bob3', 3, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', '4', 4, '{a,b}', '{}'), (5, 'u5@x.org', 'bob5', 5, NULL, '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', 'Cid6', 0, '{a,b}', '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'dee7', 7, '{a}', NULL), (8, 'u8@x.org', 'bob8', 8, '{a}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (7, 0, 'paid', '2025-02-06', '{"gift": true}'), (3, 12345678.90, 'paid', '2025-02-03', NULL), (3, 12345678.90, 'void', '2025-02-14', '{"items": []}'), (4, 100, 'new', '2025-02-02', '{"items": []}'), (3, 100, 'void', '2025-02-27', NULL), (8, 0, 'new', '2025-02-08', '{"gift": true}'), (4, 12345678.90, 'paid', '2025-02-23', '{"items": []}');

DO $$ BEGIN CREATE ROLE r7_alice; EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN CREATE ROLE r7_bob; EXCEPTION WHEN duplicate_object THEN NULL; END $$;

CREATE DOMAIN r7pos AS int CHECK (VALUE > 0);

CREATE DOMAIN r7code AS text CHECK (VALUE ~ '^[A-Z]{2}[0-9]$');

CREATE DOMAIN r7money AS numeric(12, 2) NOT NULL CHECK (VALUE >= -1000);

CREATE COLLATION r7ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r7de (provider = icu, locale = 'de-u-co-phonebk');

CREATE TABLE r7k (id int PRIMARY KEY, grp int NOT NULL, v int, note text, amt numeric(12, 3), f float8, ts timestamptz);

CREATE INDEX r7k_grp ON r7k (grp);

CREATE INDEX r7k_note ON r7k (note text_pattern_ops);

INSERT INTO r7k VALUES (1, 1, NULL, 'note 1', -12.125, 0.5, '2025-03-09 07:00:00+00'), (2, 1, 104, 'note 3', 0, 0.5, '2024-02-29 12:00:00+00'), (3, 1, -26, 'note 1', NULL, 1e300, '2025-03-09 06:59:59+00'), (4, 4, -20, 'note 3', 1000, NULL, '2025-11-02 05:30:00+00'), (5, 1, 13, 'note 1', -12.125, -0, '2024-02-29 12:00:00+00'), (6, 1, NULL, 'Note 3', -12.125, 0.5, '2025-11-02 05:30:00+00'), (7, 1, NULL, 'n%5', 1000, 0.5, '2025-03-09 06:59:59+00'), (8, 1, 30, 'note 3', 0, 'NaN', '2025-03-09 07:00:00+00'), (9, 1, 45, 'é', 0, 1e300, '2025-03-09 07:00:00+00'), (10, 1, NULL, 'note_4', 1000, 0.5, '2024-02-29 12:00:00+00'), (11, 1, -19, 'n%5', NULL, 'Infinity', '2025-11-02 05:30:00+00'), (12, 1, -10, 'note_4', 0, 1e300, '2025-11-02 05:30:00+00'), (13, 1, 194, '', 0, 'NaN', '2025-11-02 05:30:00+00'), (14, 4, NULL, 'note 2', 3.333, 1e300, '2025-03-09 06:59:59+00'), (15, 1, 23, 'note 1', -12.125, 2.25, '2024-02-29 12:00:00+00'), (16, 9, 128, 'n%5', -12.125, 2.25, '2024-02-29 12:00:00+00'), (17, 3, 126, 'note 1', 0, 'NaN', '2024-02-29 12:00:00+00'), (18, 1, -25, '', 0.5, 1e300, NULL), (19, 1, -29, 'note 1', 3.333, 1e300, NULL), (20, 1, 106, 'é', 1000, 2.25, '2025-03-09 07:00:00+00'), (21, 1, 148, '', NULL, 'Infinity', '2025-03-09 06:59:59+00'), (22, 1, 115, '', 0.5, 'Infinity', NULL), (23, 1, -27, 'note 3', 0, -0, '2025-11-02 05:30:00+00'), (24, 1, 42, 'note 3', 0.5, 0.5, NULL), (25, 4, -29, 'é', NULL, 2.25, '2025-03-09 07:00:00+00'), (26, 1, 33, 'note 2', 3.333, 2.25, '2024-02-29 12:00:00+00'), (27, 1, 76, 'Note 3', -12.125, 1e300, '2025-11-02 05:30:00+00'), (28, 1, 152, 'NOTE 3', 3.333, 0.5, '2025-11-02 05:30:00+00'), (29, 1, 94, 'note_4', 3.333, -0, '2024-02-29 12:00:00+00'), (30, 3, 114, 'note 1', NULL, 'NaN', '2024-02-29 12:00:00+00'), (31, 7, -40, 'note 3', 1000, -0, '2025-03-09 06:59:59+00'), (32, 1, 185, 'note 3', 0.5, 1e300, NULL), (33, 1, 93, 'Note 3', 0.5, 1e300, '2025-03-09 06:59:59+00'), (34, 1, -45, 'é', 0, 'NaN', '2025-03-09 07:00:00+00'), (35, 1, 193, 'NOTE 3', 0, -0, '2025-03-09 07:00:00+00'), (36, 1, -11, 'Note 3', 0, 'Infinity', '2025-03-09 06:59:59+00'), (37, 8, NULL, 'Note 3', 0.5, 0.5, '2025-03-09 07:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21);

CREATE TABLE r7r (id int NOT NULL, qty int NOT NULL, tag text, PRIMARY KEY (id, qty)) PARTITION BY RANGE (qty);

CREATE TABLE r7r_neg PARTITION OF r7r FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE r7r_low PARTITION OF r7r FOR VALUES FROM (0) TO (100);

CREATE TABLE r7r_high PARTITION OF r7r FOR VALUES FROM (100) TO (MAXVALUE);

INSERT INTO r7r VALUES (1, -5, 'a'), (2, 5, 'b'), (3, 50, NULL), (4, 150, 'd'), (5, 99, 'e');

CREATE TABLE r7g (id int GENERATED BY DEFAULT AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, 'ae', 'ae'), (2, 'ae', 'ae'), (3, 'a', 'a'), (4, 'é', 'é'), (5, 'ä', 'ä'), (6, 'ß', 'ß'), (7, 'a', 'a'), (8, 'ä b', 'ä b'), (9, 'ß', 'ß'), (10, 'ä b', 'ä b'), (11, 'b', 'b'), (12, 'Ä', 'Ä'), (13, '', ''), (14, 'Æ', 'Æ');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 1, 1, 0, NULL, '2025-01-01'::timestamp + interval '140 hours'), (2, 4, 8, 1.50, 'ab', '2025-01-01'::timestamp + interval '12 hours'), (3, 2, -5, NULL, 'z', '2025-01-01'::timestamp + interval '117 hours'), (4, 1, 14, 3.75, 'B', '2025-01-01'::timestamp + interval '57 hours'), (5, 4, NULL, -2.25, 'a', '2025-01-01'::timestamp + interval '108 hours'), (6, 3, 0, 1.50, 'B', '2025-01-01'::timestamp + interval '141 hours'), (7, 4, -1, 1.50, 'B', '2025-01-01'::timestamp + interval '94 hours'), (8, 3, 11, 1.50, 'ab', '2025-01-01'::timestamp + interval '124 hours'), (9, 3, 14, 1.50, 'a', '2025-01-01'::timestamp + interval '50 hours'), (10, 3, 14, 1.50, '', '2025-01-01'::timestamp + interval '0 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 9), (2, 3, 6), (2, 4, 1), (3, 2, 2), (3, 3, 5), (4, 3, 1), (4, 1, 3), (5, 1, 4), (5, 2, 3), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[9,)', '{[9,10), [12, 14)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (2, '[4,)', '{(,4)}', NULL, NULL), (3, NULL, '{}', 'empty', NULL), (4, NULL, '{(,10)}', '[2025-01-01,2025-02-01)', '(,0]'), (5, '[6,)', '{}', '(2024-12-31,2025-01-05]', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'a rat ran'), (2, ''), (3, 'foxes and dogs'), (4, 'running rats');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3063) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2648) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2202) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a79178 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a79178 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3163, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3163) i;

SET default_statistics_target = 25;

CREATE STATISTICS r9a79178_st (ndistinct, dependencies) ON (g + 1), b FROM r9a79178;

ANALYZE (SKIP_LOCKED) r9a79178;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a79178 WHERE f BETWEEN 20 AND 30$$);

SELECT * FROM r9est($$SELECT * FROM r9a79178 WHERE nul IS NULL$$);

DROP TABLE r9a79178;

CREATE TABLE r9i2104 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i2104 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 49) i;

ANALYZE r9i2104;

CREATE INDEX ON r9i2104 USING btree (a DESC NULLS FIRST, id);

CREATE UNIQUE INDEX r9i2104_x1 ON r9i2104 USING btree (a);

CREATE INDEX CONCURRENTLY r9i2104_x2 ON r9i2104 USING btree (abs(a - 40));

SET enable_seqscan = off;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i2104;

CREATE FUNCTION r8ef16283() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'drop % %', tg_tag, (SELECT string_agg(object_type || ':' || coalesce(object_identity, '-') || ':' || original || ':' || normal || ':' || is_temporary, ', ' ORDER BY object_type, object_identity) FROM pg_event_trigger_dropped_objects() WHERE schema_name IS DISTINCT FROM 'pg_toast'); END $$;

CREATE EVENT TRIGGER r8ef16283 ON sql_drop EXECUTE FUNCTION r8ef16283();

CREATE TABLE r8e16283 (a int PRIMARY KEY, b text);

CREATE INDEX r8e16283_b ON r8e16283 (b);

ALTER TABLE r8e16283 ADD COLUMN c int DEFAULT 1;

ALTER TABLE r8e16283 ALTER COLUMN a TYPE bigint;

ALTER TABLE r8e16283 ADD COLUMN d timestamptz DEFAULT clock_timestamp();

CREATE VIEW r8e16283_v AS SELECT a FROM r8e16283;

ALTER TABLE r8e16283 DROP COLUMN b;

DROP TABLE r8e16283 CASCADE;

SELECT evtname, evtevent, evtenabled, evttags FROM pg_event_trigger WHERE evtname = 'r8ef16283';

DROP EVENT TRIGGER r8ef16283;

DROP FUNCTION r8ef16283;

CREATE TABLE r9c39067 (id int PRIMARY KEY, a int, b text, s serial) WITH (autovacuum_enabled = off);

INSERT INTO r9c39067 (id, a, b) VALUES (7, 1, 'pre'), (1200, 2, 'pre2');

BEGIN;

COPY r9c39067 (id, a, b) FROM STDIN (FORMAT csv);
1,2,b1
2,9,b2
3,16,b3
2,23,b4
\.

\s2 SELECT count(*), max(id) FROM r9c39067;

COMMIT;

\s2 SELECT count(*), max(id), sum(a) FROM r9c39067;

DROP TABLE r9c39067;

\s3 UPDATE r7k SET v = 7 WHERE id = 11 RETURNING id, v;

\x3 rf=1,0 p=t'7' p=t'11'
UPDATE r7k SET v = $1 WHERE id = $2 RETURNING id, v;

\s2 UPDATE r7k SET v = -1 WHERE id = 6 RETURNING id, v;

UPDATE r7k SET v = 2147483647 WHERE id = 5 RETURNING id, v;

\x2 types=23,23 describe p=b7fffffff p=b00000005
UPDATE r7k SET v = $1 WHERE id = $2 RETURNING id, v;

\x3 name=r7n25083 types=23,23 describe rf=0 p=b7fffffff p=t'5'
UPDATE r7k SET v = $1 WHERE id = $2 RETURNING id, v;

\x3 name=r7n25083 rows=2 p=t'-1' p=b00000003
-;

\x3 name=r7n25083 p=t'42' p=t'2'
-;

\x3 name=r7n25083 rf=1,0 p=b80000000 p=t'6'
-;

\x3 name=r7n25083 p=b00000000 p=b00000004
-;

\x3 name=r7n25083 rows=2 p=t'1' p=t'99'
-;

\s3 DEALLOCATE r7n25083;

CREATE TABLE r9i14858 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i14858 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 154) i;

ANALYZE r9i14858;

CREATE INDEX r9i14858_x0 ON r9i14858 USING btree (t COLLATE "C");

CREATE INDEX r9i14858_x1 ON r9i14858 USING btree (t text_pattern_ops);

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i14858 WHERE p <@ box '((0,0),(5,5))';

EXPLAIN (COSTS OFF) SELECT id, a FROM r9i14858 WHERE a < 89 ORDER BY a DESC NULLS FIRST, id LIMIT 10;

EXPLAIN (COSTS OFF) SELECT id, a FROM r9i14858 WHERE a < 57 ORDER BY a DESC NULLS FIRST, id LIMIT 10;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i14858;

\x2 name=r9x5172 types=23,23 describe rf=0 p=t'2147483647' p=t'1000'
SELECT id, x FROM r9s WHERE id BETWEEN $1 AND $1 + $2 ORDER BY id DESC LIMIT 20;

\x2 name=r9x5172 p=t'2' p=b00000000
-;

\x2 name=r9x5172 p=t'2147483647' p=t'1'
-;

\x2 name=r9x5172 p=t'-5' p=t'5'
-;

\x2 name=r9x5172 p=t'1' p=b00000000
-;

\x2 name=r9x5172 rf=1 p=t'2147483647' p=t'5'
-;

\x2 name=r9x5172 p=b00000064 p=t'5'
-;

\x2 name=r9x5172 rows=2 p=b00000032 p=t'1000'
-;

\x2 name=r9x5172 rf=1 p=t'-5' p=t'1000'
-;

\x2 name=r9x5172 rf=1 p=t'-5' p=b000003e8
-;

\x2 name=r9x5172 rf=0 p=b00000000 p=t'-1'
-;

\x2 name=r9x5172 rf=0 p=t'2' p=t'-1'
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x5172';

\s2 DEALLOCATE r9x5172;

\s2 RESET plan_cache_mode;

BEGIN;

DECLARE r6k NO SCROLL CURSOR FOR SELECT x.id, y.w FROM r5a x JOIN r5b y ON y.k = x.id;

SELECT name, is_holdable, is_scrollable FROM pg_cursors ORDER BY name;

SAVEPOINT r6f;

FETCH LAST FROM r6k;

ROLLBACK TO SAVEPOINT r6f;

SAVEPOINT r6f;

MOVE LAST IN r6k;

ROLLBACK TO SAVEPOINT r6f;

SAVEPOINT r6f;

DELETE FROM r5a WHERE CURRENT OF r6k RETURNING id;

RELEASE SAVEPOINT r6f;

SAVEPOINT r6f;

FETCH LAST FROM r6k;

ROLLBACK TO SAVEPOINT r6f;

COMMIT;

FETCH ALL FROM r6k;

CLOSE ALL;

CREATE TABLE r9w76964 (a int, b text);

INSERT INTO r9w76964 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q76964 AS SELECT * FROM r9w76964 WHERE a >= $1 ORDER BY a;

EXECUTE r9q76964(1);

EXECUTE r9q76964(2);

ALTER TABLE r9w76964 ALTER COLUMN a SET DEFAULT 5;

EXECUTE r9q76964(1);

DEALLOCATE r9q76964;

DROP TABLE r9w76964;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = force_custom_plan; FOR i IN 0..7 LOOP s := s + coalesce((SELECT count(*) FROM r9s WHERE g = i % 4 + 1), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

CREATE TABLE r9w96841 (a int, b text);

INSERT INTO r9w96841 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q96841 AS SELECT * FROM r9w96841 WHERE a >= $1 ORDER BY a;

EXECUTE r9q96841(1);

EXECUTE r9q96841(2);

ALTER TABLE r9w96841 ALTER COLUMN a SET DEFAULT 5;

EXECUTE r9q96841(1);

DEALLOCATE r9q96841;

DROP TABLE r9w96841;

SET enable_indexonlyscan = off;

RESET enable_indexonlyscan;

CREATE TABLE r9a99924 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a99924 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2568, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2568) i;

ALTER TABLE r9a99924 ALTER COLUMN u SET STATISTICS -1;

SET default_statistics_target = 25;

ALTER TABLE r9a99924 ALTER COLUMN u SET (n_distinct = 0);

ANALYZE r9a99924;

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a99924 WHERE arr @> '{2}'$$);
