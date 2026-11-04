-- class: wrong_rows
-- ordered: false
-- note: 1 vs 1 rows
-- engine: name:25 | generic_plans:20 | custom_plans:20 | from_sql:16 | parameter_types:25 | result_types:25
-- engine: r9p29104 | 0 | 7 | t | {integer} | {integer,bigint,numeric}
-- engine: -- SELECT 1
-- reference: name:25 | generic_plans:20 | custom_plans:20 | from_sql:16 | parameter_types:25 | result_types:25
-- reference: r9p29104 | 1 | 6 | t | {integer} | {integer,bigint,numeric}
-- reference: -- SELECT 1

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a date, b numeric(10,2), c int4, d int8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL) PARTITION BY RANGE (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO ('2024-02-01');

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b, c, d) VALUES (27, '2024-01-17', 6.25, 2, NULL), (16, NULL, 6.75, 9, 16), (31, '2024-02-20', 8.05, 1, 3), (5, '2024-01-02', NULL, 5, 20), (21, NULL, 0.00, 1, 7), (35, '2024-02-07', 8.50, 4, 9), (8, NULL, 9.00, NULL, NULL), (15, '2024-02-14', NULL, 6, 6);

CREATE INDEX t0_i412 ON t0 (c, d);

CREATE INDEX t0_i929 ON t0 ((c + 1));

CREATE TABLE t1 (id int4 PRIMARY KEY, a int2, b text COLLATE "und-x-icu");

INSERT INTO t1 (id, a, b) VALUES (36, NULL, 'x '), (40, NULL, NULL), (21, NULL, 'Z'), (7, 2, 'Z'), (20, NULL, 'B'), (14, NULL, NULL), (23, NULL, NULL), (15, NULL, NULL), (33, NULL, NULL), (10, 5, NULL), (24, NULL, NULL), (22, NULL, 'z'), (5, NULL, NULL), (39, 3, NULL), (28, 2, NULL), (13, NULL, NULL), (25, NULL, 'a%'), (55, NULL, NULL), (8, NULL, NULL), (34, NULL, NULL), (26, NULL, NULL), (3, NULL, NULL), (27, NULL, NULL), (35, 0, '9'), (1, NULL, NULL), (12, NULL, NULL), (48, NULL, 'Ä'), (57, 5, ''), (4, NULL, NULL), (31, NULL, '9');

CREATE TABLE t2 (id int4, a float8, b bool, c date, d int4, e timestamp, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (997, -1, true, '2024-02-04', NULL, '2024-01-05 23:00:00', 2);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (808, -1.75, false, '2024-01-21', 6, '2024-01-09 00:30:00', 3);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (747, 1, NULL, NULL, NULL, '2024-01-01 00:00:00', 2);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (36, -1.25, false, '2024-02-06', 5, '2024-01-05 06:00:00'), (58, -1.5, NULL, '2024-02-07', NULL, '2024-01-04 06:00:00'), (19, 2, true, '2024-03-16', 0, '2024-01-02 12:30:00'), (16, 2.5, true, '2024-01-11', NULL, NULL), (21, 2.5, false, '2024-01-25', 9, '2024-01-03 06:00:00'), (7, 0.75, NULL, '2024-02-15', -2, '2024-01-07 00:00:00'), (10, 0.75, true, NULL, 5, '2024-01-08 00:00:00'), (40, NULL, true, '2024-01-23', 4, '2024-01-02 00:30:00'), (30, 3.75, false, '2024-03-27', NULL, NULL), (29, 1.25, false, '2024-01-10', 1, '2024-01-05 23:30:00'), (14, 2.25, true, '2024-01-01', 8, '2024-01-02 06:00:00'), (37, 0, true, '2024-02-08', -1, '2024-01-01 12:00:00'), (17, 2, true, '2024-01-08', 8, '2024-01-09 00:00:00'), (5, -0.75, true, NULL, NULL, '2024-01-06 12:00:00'), (27, 3, false, NULL, NULL, '2024-01-02 00:30:00'), (47, 3, false, '2024-02-27', 3, '2024-01-02 12:00:00'), (1, 1.5, false, '2024-02-16', NULL, '2024-01-07 00:30:00'), (51, 0.5, false, '2024-02-18', NULL, '2024-01-07 12:00:00'), (25, 3.25, true, NULL, 0, NULL), (59, 1, true, '2024-01-14', 1, '2024-01-01 00:00:00'), (50, 2.75, true, '2024-03-23', 2, '2024-01-06 23:00:00'), (28, 1.25, false, NULL, 3, '2024-01-02 06:00:00'), (3, 3.75, true, '2024-02-27', -2, '2024-01-05 23:30:00'), (33, 0.5, NULL, '2024-01-07', NULL, '2024-01-04 06:30:00'), (4, NULL, false, '2024-01-10', 9, NULL), (12, 3.25, true, NULL, -1, '2024-01-08 00:30:00'), (39, NULL, NULL, '2024-02-09', -1, '2024-01-01 06:30:00'), (13, 0.25, false, '2024-02-20', 5, '2024-01-09 12:30:00'), (57, -1, true, '2024-03-27', -2, '2024-01-09 06:30:00'), (48, -1.75, false, '2024-01-03', 1, '2024-01-06 00:00:00');

CREATE INDEX t2_i628 ON t2 USING hash (b);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int4[], b text COLLATE "C", c timestamp, d text[], e date);

INSERT INTO t3 (id, a, b, c, d, e) VALUES (9, '{1,1,2}', NULL, '2024-01-09 00:00:00', NULL, '2024-02-15'), (18, '{}', NULL, '2024-01-07 06:00:00', NULL, NULL), (23, '{3,1,2}', NULL, '2024-01-09 00:00:00', NULL, NULL), (10, NULL, NULL, '2024-01-03 00:30:00', NULL, NULL), (15, NULL, NULL, NULL, NULL, NULL), (26, NULL, NULL, '2024-01-06 00:00:00', '{b,NULL}', '2024-02-08'), (35, NULL, NULL, NULL, NULL, '2024-03-21'), (39, NULL, '10', '2024-01-05 12:30:00', '{A,a}', '2024-02-27'), (20, '{}', NULL, NULL, NULL, NULL), (19, NULL, NULL, NULL, NULL, NULL), (32, '{1,2}', NULL, '2024-01-01 12:30:00', '{}', '2024-03-09'), (8, '{1,2}', NULL, NULL, '{}', NULL), (27, NULL, NULL, NULL, NULL, NULL), (16, NULL, NULL, '2024-01-07 06:30:00', NULL, '2024-02-19'), (21, NULL, 'ab', NULL, '{}', NULL), (33, NULL, NULL, NULL, '{a,b}', '2024-02-12'), (25, NULL, NULL, NULL, NULL, NULL), (34, NULL, 'Ä', NULL, NULL, NULL), (4, NULL, NULL, NULL, NULL, '2024-02-16'), (3, NULL, NULL, NULL, '{A,a}', NULL);

CREATE INDEX t3_i938 ON t3 (e DESC NULLS LAST);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t3 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t3 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t3 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t3 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "C", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('b', 2)::ct3, ARRAY[ROW('Ä', 2)::ct3, ROW('Ä', 3)::ct3], 1), (2, ROW('é', 3)::ct3, ARRAY[ROW('foo', -1)::ct3, ROW('foo', 3)::ct3], 2), (3, ROW('10', 2)::ct3, ARRAY[ROW('ab', 2)::ct3, ROW('b', 0)::ct3], 4), (4, ROW('B', 2)::ct3, ARRAY[ROW('x ', 3)::ct3, ROW('aa', 2)::ct3], NULL), (5, ROW('A', 1)::ct3, ARRAY[ROW(NULL, 0)::ct3, ROW('Z', 2)::ct3], 4), (6, ROW('foo', 0)::ct3, ARRAY[ROW('x ', NULL)::ct3, ROW(NULL, 2)::ct3], 2), (7, ROW(NULL, NULL)::ct3, ARRAY[ROW('z', NULL)::ct3, ROW(NULL, 0)::ct3], 3), (8, ROW('Ä', 2)::ct3, '{}', 0);

CREATE INDEX tc3_c ON tc3 (c);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF isfinite('2025-10-16'::date) THEN
    r := array_length('{1,1,2}'::int4[], 1);
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce((SELECT (count(*))::int FROM t3 t1 WHERE JSON_EXISTS(jsonb_build_object('k', 12, 'v', 'ba'), 'strict $.a.b')), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, b FROM t3 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, b FROM t3 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT (t3.id)::int2 AS c0, '2024-05-16 05:00:00+00'::timestamptz AS c1, ((('{"a": 1, "b": "x"}'::jsonb -> 'b') || ('{"a": 1, "b": "x"}'::jsonb - 'a')) - 'a') AS c2 FROM ((SELECT -7.125::numeric AS c0 FROM t1 t2) s1 LEFT JOIN t3 t3 ON repeat(t3.b, 2) !~ '%b') WHERE COALESCE((true OR true), false);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', 'infinity', '2025-10-26 02:30:00', '2000-03-01', '2 weeks 3 microseconds', NULL, '0.5', -2), (2, NULL, NULL, '0044-03-15 12:00:00+00 BC', NULL, '-infinity', '1 year 2 mons', 'NaN', 'NaN', 8), (3, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', 'infinity', '2025-01-01', '1 mon -30 days', '-1e-20', '-1.25', 7), (4, '"str"', '"str"', '2025-11-02 01:30:00-04', '2025-12-31 23:59:59.999999', NULL, '36 hours', NULL, '0.5', 1), (5, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-11-02 01:30:00-05', '1970-01-01 00:00:00.5', '-infinity', '23:59:59.999999', '-2.5', 'NaN', NULL), (6, NULL, NULL, NULL, '2025-03-09 02:30:00', '2000-03-01', 'infinity', '-0.0', '-1.25', 2), (7, '"str"', '"str"', NULL, NULL, NULL, NULL, NULL, '-3', 2), (8, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-11-02 01:30:00-04', '0001-01-01 00:00:00 BC', '1999-12-31', '-12:00', '0.000001', '-1.25', 0), (9, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, NULL, '1999-12-31', NULL, '0', NULL, 7), (10, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', '1970-01-01 00:00:00.5', '-infinity', '1 mon -30 days', '1.23456789012345678901234567890', '-1.25', 6);

INSERT INTO tp4 VALUES (23, '2025-01-01', 'c', '-Infinity'), (0, 'infinity', 'a', '-Infinity'), (10, '-infinity', 'a', '1.23456789012345678901234567890'), (1, '2025-03-09', 'd', NULL), (12, '2025-01-01', 'a', '1.23456789012345678901234567890'), (-1, '1999-12-31', 'd', '-0.0'), (12, '-infinity', 'd', '-Infinity'), (22, '2025-03-09', NULL, '0');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "unicode", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 12, 'B', '123456789012345678901234567890.123456789', '30 days', '(1,5]', '{[2,4)}', 'running runners ran; the runner runs', ARRAY[ROW(5, 'I')::r5ct]), (2, 8, '9', '2.5', '2147483647 mons', '(,2]', NULL, 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(3, NULL)::r5ct, ROW(NULL, 'ss')::r5ct]), (3, 20, 'ss', '-Infinity', '720:00:00', '[3,)', '{[0,100)}', 'the and of', '{}'), (4, NULL, 'c', '7', '720:00:00', 'empty', '{[1,3),[5,7)}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (5, 19, 'a', '-99999999999999999999999999999999999.999999', '1 year -1 sec', '[0,10)', '{[1,2],[2,3)}', '', '{}');

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 8 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (2, 'a', NULL), (5, 'b', NULL), (9, 'c', '2025-01-01'), (8, 'd', 'infinity'), (7, 'a', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (24, 'x0', NULL), (32, 'x1', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, '{}', '{}'), (2, 'u2@x.org', 'zoë2', 2, NULL, '[1, {"a": null}]'), (3, 'u3@x.org', 'Cid3', 3, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', '4', 4, '{a}', '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (4, 9.99, 'void', '2025-02-07', NULL), (4, 9.99, 'new', '2025-02-03', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 100, 'paid', '2025-02-03', '{"items": []}'), (1, 9.99, 'void', '2025-02-28', NULL), (2, 0, 'paid', '2025-02-02', NULL), (3, 12345678.90, 'void', '2025-02-27', '{"items": [{"sku": "a", "qty": 2}]}'), (1, -1.50, 'new', '2025-02-14', '{"gift": true}'), (2, 100, 'paid', '2025-02-13', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 12345678.90, 'paid', '2025-02-16', '{"items": []}'), (2, 0, 'paid', '2025-02-25', '{"items": []}');

ANALYZE r6u;

ANALYZE r6o;

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

INSERT INTO r7k VALUES (1, 1, -45, 'é', -12.125, 2.25, '2025-11-02 05:30:00+00'), (2, 1, NULL, 'NOTE 3', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (3, 9, -19, 'note_4', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (4, 7, -39, 'Note 3', 3.333, 1e300, '2025-03-09 07:00:00+00'), (5, 1, 187, '', 0, 0.5, '2024-02-29 12:00:00+00'), (6, 1, 147, 'n%5', NULL, 0.5, '2025-11-02 05:30:00+00'), (7, 1, NULL, 'n%5', 0, NULL, '2025-11-02 05:30:00+00'), (8, 6, 43, 'n%5', NULL, 1e300, '2025-03-09 06:59:59+00'), (9, 4, 74, 'note 2', 0.5, 2.25, '2025-11-02 05:30:00+00'), (10, 1, 74, 'é', 0.5, NULL, '2024-02-29 12:00:00+00'), (11, 5, -47, 'note 1', -12.125, -0, '2024-02-29 12:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10);

CREATE TABLE r7r (id int NOT NULL, qty int NOT NULL, tag text, PRIMARY KEY (id, qty)) PARTITION BY RANGE (qty);

CREATE TABLE r7r_neg PARTITION OF r7r FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE r7r_low PARTITION OF r7r FOR VALUES FROM (0) TO (100);

CREATE TABLE r7r_high PARTITION OF r7r FOR VALUES FROM (100) TO (MAXVALUE);

INSERT INTO r7r VALUES (1, -5, 'a'), (2, 5, 'b'), (3, 50, NULL), (4, 150, 'd'), (5, 99, 'e');

CREATE TABLE r7g (id int GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, 'Ä', 'Ä'), (2, 'é', 'é'), (3, 'ß', 'ß'), (4, 'z', 'z'), (5, 'Ä', 'Ä'), (6, 'a', 'a'), (7, 'ß', 'ß'), (8, 'e', 'e'), (9, 'Ä', 'Ä'), (10, 'é', 'é'), (11, 'ß', 'ß'), (12, '', '');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 1, -5, -2.25, NULL, '2025-01-01'::timestamp + interval '3 hours'), (2, 4, 7, -2.25, 'ab', '2025-01-01'::timestamp + interval '36 hours'), (3, 4, 11, NULL, 'ab', '2025-01-01'::timestamp + interval '74 hours'), (4, 1, 18, 1.50, '', '2025-01-01'::timestamp + interval '55 hours'), (5, 1, -1, 1.50, 'z', '2025-01-01'::timestamp + interval '192 hours'), (6, 1, 9, NULL, 'b', '2025-01-01'::timestamp + interval '12 hours'), (7, 3, 20, 0, NULL, '2025-01-01'::timestamp + interval '46 hours'), (8, 2, 4, NULL, 'a', '2025-01-01'::timestamp + interval '54 hours'), (9, 2, -2, 1.50, 'B', '2025-01-01'::timestamp + interval '42 hours'), (10, 2, 16, -2.25, 'b', '2025-01-01'::timestamp + interval '122 hours'), (11, 1, 12, 1.50, '', '2025-01-01'::timestamp + interval '93 hours'), (12, 4, -3, 10, 'a', '2025-01-01'::timestamp + interval '48 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (3, 1, 3), (3, 4, 5), (4, 1, 2), (5, 2, 6), (5, 5, 5), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[6,)', '{}', NULL, '(,0]'), (2, NULL, '{(,1)}', 'empty', '[0,0]'), (3, 'empty', '{(,9)}', 'empty', '[1.5,2.5)'), (4, '[6,6)', '{[6,6), [8, 10)}', '[2025-01-01,2025-02-01)', '(,0]'), (5, '[5,5)', '{}', '[2025-03-01,infinity)', '(,0]'), (6, 'empty', '{(,5)}', NULL, '[0,0]'), (7, '[-2,2)', '{(,-2)}', NULL, NULL);

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'dog-eat-dog world'), (2, 'quick quick slow'), (3, 'running rats'), (4, 'the quick brown fox'), (5, 'running rats'), (6, 'jumps over the lazy dog'), (7, 'foxes and dogs'), (8, 'a rat ran'), (9, '');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '"scalar"'), (4, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3564) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1722) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2969) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

PREPARE r9p29104 (int) AS SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

EXPLAIN (COSTS OFF) EXECUTE r9p29104(-1);

EXECUTE r9p29104(-1);

EXECUTE r9p29104(-50);

EXECUTE r9p29104(NULL);

EXECUTE r9p29104(3000000000);

EXECUTE r9p29104(0);

EXECUTE r9p29104(3000000000);

EXECUTE r9p29104(3000000000);

EXPLAIN (COSTS OFF) EXECUTE r9p29104(7);

EXECUTE r9p29104(7);

-- statement
SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p29104';
