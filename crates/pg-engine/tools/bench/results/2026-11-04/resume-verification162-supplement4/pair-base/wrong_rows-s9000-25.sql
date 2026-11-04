-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: r9est:25
-- engine: HashAggregate  (rows=1600)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a60687  (rows=3195)
-- engine: -- SELECT 3
-- reference: r9est:25
-- reference: HashAggregate  (rows=3195)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a60687  (rows=3195)
-- reference: -- SELECT 3

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (a numeric(10,2), b d3text);

INSERT INTO t0 (a, b) VALUES (6.75, 'abc'), (NULL, NULL), (10.05, 'B'), (12.50, 'ba'), (-2.00, 'ba'), (5.05, '10'), (8.50, 'é'), (10.75, 'Ä'), (12.00, NULL), (9.75, NULL), (4.50, 'Z'), (1.00, 'abc'), (NULL, NULL), (12.50, '9'), (2.75, 'a%'), (3.25, 'A'), (3.00, 'a%'), (0.25, 'é'), (12.00, 'foo'), (2.75, 'z');

CREATE TABLE t1 (a date, b date, c int2, d bool, e interval);

CREATE TABLE t1_c (extra int) INHERITS (t1);

INSERT INTO t1_c (a, b, c, d, e, extra) VALUES ('2024-02-07', '2024-02-06', 1, false, '-1 day', 2);

INSERT INTO t1_c (a, b, c, d, e, extra) VALUES ('2024-01-19', NULL, 5, false, '1 mon', 1);

INSERT INTO t1 (a, b, c, d, e) VALUES ('2024-01-13', NULL, NULL, false, NULL), ('2024-03-22', NULL, 0, false, '30 days'), ('2024-03-14', '2024-01-22', 5, true, '2 hours'), ('2024-03-12', NULL, NULL, true, '0'), ('2024-01-14', '2024-03-12', 1, false, '1 day'), ('2024-03-23', '2024-01-05', 1, true, '30 days'), ('2024-01-01', '2024-03-19', 5, true, '24:00:00'), ('2024-02-12', '2024-01-15', 0, false, '30 days'), ('2024-01-06', '2024-01-02', 6, true, '24:00:00'), ('2024-02-15', '2024-01-07', 3, false, '30 days'), ('2024-01-19', NULL, NULL, true, '1 day'), ('2024-03-04', '2024-03-08', 3, false, '0');

CREATE INDEX t1_i520 ON t1 (a) WHERE a IS NOT NULL;

CREATE INDEX t1_i146 ON t1 (b, e);

CREATE INDEX t1_i373 ON t1 (e);

CREATE TABLE t2 (id int4 PRIMARY KEY, a float8, b text COLLATE "und-x-icu", c d3int, d d3text, e timestamp);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (39, 2, 'ß', 6, 'aa', '2024-01-07 23:00:00'), (4, NULL, 'Z', 9, NULL, NULL), (34, 1.75, NULL, -1, 'foo', NULL), (32, 3.75, NULL, NULL, NULL, '2024-01-03 12:30:00'), (29, 2.5, 'b', NULL, '9', NULL), (7, 0, NULL, 7, 'ß', '2024-01-07 12:00:00'), (27, NULL, 'b', NULL, 'bar', NULL), (16, NULL, ' x', NULL, 'Z', NULL), (36, -1.25, NULL, 1, ' x', '2024-01-06 06:30:00'), (35, 2.5, NULL, -1, 'bar', NULL), (33, NULL, NULL, 8, 'é', '2024-01-09 23:30:00'), (18, 4, 'foo', 7, NULL, '2024-01-08 00:30:00');

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; NEW.c := coalesce(NEW.c, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t2 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

ANALYZE;

CREATE VIEW v0 AS SELECT (overlay(('2000-09-27 18:00:00-05'::timestamptz AT TIME ZONE 'EST5EDT')::text placing 'zz' from 2 for 1) COLLATE "und-x-icu") AS c0, sign(s1.g)::int AS c1, CASE WHEN ('{}'::int4multirange @> LEAST(s1.g, s1.g)) THEN lpad(lpad('A', 3, '*'), 3, '*') ELSE '10' END AS c2, COALESCE('2 hours'::interval, ('-1 mon'::interval + ('1 mon -1 day'::interval)::text::interval)) AS c3 FROM generate_series(1, 0) s1(g) WHERE true;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', '2000-01-01 00:00:00', '2024-02-29', '2 weeks 3 microseconds', 'Infinity', '-1.25', 2), (2, '{}', '{}', '2025-10-26 00:30:00+00', NULL, '2000-03-01', '1 day', NULL, 'NaN', 11), (3, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-10-26 00:30:00+00', '2024-02-29 00:00:00', '-infinity', '36 hours', '1e30', 'NaN', 6), (4, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '2025-11-02 01:30:00', '1999-12-31', '-1 mon 2 days', NULL, '-0', 4), (5, '{}', '{}', '2025-11-02 01:30:00-05', '2024-02-29 00:00:00', NULL, NULL, NULL, '0.125', 8), (6, 'null', 'null', '1999-12-31 23:59:59.999999+00', '2025-10-26 02:30:00', NULL, '2 weeks 3 microseconds', '123.45', '-1.25', -1), (7, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', NULL, '2000-01-01 00:00:00', '2025-01-01', '1 day', '0', '-0', -3), (8, '"str"', '"str"', '0044-03-15 12:00:00+00 BC', '2025-03-09 02:30:00', '2025-01-01', '0', '-0.0', '2', 6), (9, 'null', 'null', '-infinity', '2000-01-01 00:00:00', 'infinity', '1 mon -30 days', '0.000001', '0.5', 5), (10, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', '2025-03-09 02:30:00', 'infinity', '-12:00', '1.23456789012345678901234567890', 'NaN', -2), (11, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '1999-12-31 23:59:59.999999+00', NULL, '2000-03-01', '23:59:59.999999', '-7', NULL, 3), (12, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '1970-01-01 00:00:00.5', '2025-01-01', '1 year 2 mons', '-0.0', '-0', 10);

INSERT INTO tp4 VALUES (-4, NULL, 'a', 'Infinity'), (0, '2025-03-09', 'a', '2.5'), (0, '2025-03-09', 'b', 'NaN'), (8, 'infinity', NULL, '2.5'), (-5, '-infinity', 'a', '0.5'), (2, '2024-02-29', NULL, 'Infinity'), (25, NULL, NULL, 'Infinity'), (21, '2024-02-29', 'ab', '1e30'), (23, '2024-02-29', 'd', '1e30'), (22, '2000-03-01', 'b', '0'), (13, '4713-01-01 BC', 'd', '-0.0'), (7, '2000-03-01', 'b', NULL), (24, '2000-03-01', NULL, 'Infinity'), (9, 'infinity', 'd', '0.5'), (2, '2000-03-01', 'a', '0.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "unicode", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, -1, 'b', '0', '1 year -1 sec', '(,)', '{[2,4)}', '', ARRAY[ROW(3, 'o')::r5ct]), (2, 1, 'é', '-Infinity', '2147483647 mons', '[0,10)', '{[0,100)}', 'running runners ran; the runner runs', ARRAY[ROW(0, 'AB')::r5ct, ROW(1, 'SS')::r5ct]), (3, 3, 'E', '0', '-178000000 years', '(1,5]', '{[1,2],[2,3)}', '', ARRAY[ROW(4, 'å')::r5ct]), (4, 6, 'ae', 'NaN', '-1 mon 30 days', '[0,10)', '{}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(3, 'o')::r5ct, ROW(4, 'Ab')::r5ct]), (5, 13, 'ö', '123456789012345678901234567890.123456789', '1 day 00:00:00.000001', NULL, '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(3, 'B')::r5ct]), (6, 20, 'ss', '0.0000000000000000000000000001', '-infinity', NULL, '{empty}', 'the and of', ARRAY[ROW(2, 'ı')::r5ct]), (7, NULL, 'E', '1e-130', '1 day 00:00:00.000001', '[5,6)', '{[1,3),[5,7)}', 'running runners ran; the runner runs', ARRAY[ROW(NULL, 'Z')::r5ct]);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 25 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (6, 'a', '2024-02-29'), (7, 'b', '2025-01-01'), (-1, 'c', '2025-01-01'), (18, 'd', 'infinity'), (12, 'a', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL), (26, 'x1', NULL), (25, 'x2', NULL), (20, 'x3', NULL), (21, 'x4', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 0, '{a}', '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'Éva2', 0, '{a}', '[1, {"a": null}]'), (3, 'u3@x.org', 'ann3', 3, '{a,b}', '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', 'Cid4', 0, '{b,NULL}', '{"seats": 1.50}'), (5, 'u5@x.org', 'ann5', 5, '{}', '{}'), (6, 'u6@x.org', 'ann6', 0, '{b,NULL}', '{}'), (7, 'u7@x.org', 'ann7', 0, '{a,b}', '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, 9.99, 'new', '2025-02-22', '{"items": []}'), (7, 100, 'paid', '2025-02-11', NULL), (6, 100, 'new', '2025-02-10', NULL), (4, 12345678.90, 'void', '2025-02-12', NULL), (5, 0, 'paid', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 100, 'paid', '2025-02-06', '{"gift": true}'), (4, -1.50, 'paid', '2025-02-14', '{"items": []}'), (3, NULL, 'paid', '2025-02-22', '{"items": []}'), (4, 9.99, 'new', '2025-02-26', NULL), (7, 9.99, 'paid', '2025-02-10', NULL), (1, -1.50, 'new', '2025-02-10', NULL), (1, NULL, 'paid', '2025-02-16', NULL), (5, -1.50, 'new', '2025-02-06', '{"gift": true}'), (7, 100, 'void', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 0, 'void', '2025-02-09', '{"gift": true}'), (2, 0, 'new', '2025-02-06', NULL), (5, 9.99, 'void', '2025-02-01', NULL), (7, 12345678.90, 'void', '2025-02-01', '{"items": []}'), (6, 12345678.90, 'new', '2025-02-08', '{"gift": true}'), (3, NULL, 'new', '2025-02-11', '{"items": [{"sku": "a", "qty": 2}]}'), (2, -1.50, 'void', '2025-02-16', '{"gift": true}'), (7, -1.50, 'paid', '2025-02-14', '{"items": [{"sku": "a", "qty": 2}]}'), (4, -1.50, 'void', '2025-02-15', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, -39, '', 3.333, 'NaN', '2025-03-09 07:00:00+00'), (2, 1, 167, 'NOTE 3', NULL, NULL, '2025-03-09 06:59:59+00'), (3, 1, 148, 'n%5', NULL, 'NaN', '2024-02-29 12:00:00+00'), (4, 1, 113, 'é', 1000, 'Infinity', '2025-11-02 05:30:00+00'), (5, 1, NULL, 'note 2', -12.125, 2.25, '2025-03-09 07:00:00+00'), (6, 1, 162, 'n%5', NULL, 1e300, '2025-11-02 05:30:00+00'), (7, 1, 99, '', NULL, -0, '2024-02-29 12:00:00+00'), (8, 1, 77, 'note 2', 0, -0, '2025-03-09 06:59:59+00'), (9, 6, 136, 'Note 3', NULL, 'Infinity', NULL), (10, 1, 133, 'NOTE 3', 0, -0, '2024-02-29 12:00:00+00'), (11, 1, -23, 'note_4', 1000, 'Infinity', '2024-02-29 12:00:00+00'), (12, 1, 78, '', 0.5, 'Infinity', NULL), (13, 6, -46, 'note 2', NULL, 0.5, '2024-02-29 12:00:00+00'), (14, 1, -13, 'Note 3', 0.5, 2.25, '2025-03-09 07:00:00+00'), (15, 7, 179, 'note 1', NULL, NULL, NULL), (16, 1, NULL, 'NOTE 3', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (17, 1, NULL, 'note_4', -12.125, 'Infinity', NULL), (18, 1, 169, 'Note 3', NULL, 'Infinity', NULL), (19, 6, 25, 'Note 3', 1000, 1e300, NULL), (20, 1, 102, 'n%5', NULL, 'NaN', NULL), (21, 5, 103, '', -12.125, 'NaN', '2024-02-29 12:00:00+00'), (22, 7, NULL, 'note 1', 0, 0.5, '2024-02-29 12:00:00+00'), (23, 1, 176, 'NOTE 3', 1000, -0, '2025-03-09 06:59:59+00'), (24, 1, -43, 'é', 1000, -0, '2025-11-02 05:30:00+00'), (25, 3, NULL, 'note_4', 1000, 0.5, '2025-03-09 06:59:59+00'), (26, 1, 59, 'NOTE 3', 1000, 1e300, NULL), (27, 1, 191, 'Note 3', 3.333, NULL, NULL), (28, 1, -29, 'note_4', 0.5, 1e300, '2025-03-09 06:59:59+00'), (29, 1, 154, 'note 1', 3.333, 1e300, NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19);

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

INSERT INTO r7t VALUES (1, 'ß', 'ß'), (2, 'Æ', 'Æ'), (3, 'b', 'b'), (4, 'Ä', 'Ä'), (5, '', ''), (6, 'a', 'a'), (7, 'e', 'e'), (8, 'b', 'b'), (9, 'b', 'b'), (10, '9', '9'), (11, 'é', 'é'), (12, 'Ä', 'Ä'), (13, 'a', 'a');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, 11, NULL, 'b', '2025-01-01'::timestamp + interval '93 hours'), (2, 1, -3, 1.50, 'ab', '2025-01-01'::timestamp + interval '192 hours'), (3, 3, NULL, 3.75, 'B', '2025-01-01'::timestamp + interval '189 hours'), (4, 3, 11, 3.75, 'a', '2025-01-01'::timestamp + interval '0 hours'), (5, 1, 6, NULL, '', '2025-01-01'::timestamp + interval '48 hours'), (6, 2, -2, 10, 'z', '2025-01-01'::timestamp + interval '123 hours'), (7, 2, 3, NULL, NULL, '2025-01-01'::timestamp + interval '114 hours'), (8, 3, 8, 1.50, NULL, '2025-01-01'::timestamp + interval '114 hours'), (9, 1, NULL, 1.50, 'B', '2025-01-01'::timestamp + interval '201 hours'), (10, 3, 7, 0, 'B', '2025-01-01'::timestamp + interval '52 hours'), (11, 1, 12, 3.75, 'ab', '2025-01-01'::timestamp + interval '210 hours'), (12, 1, 0, 10, NULL, '2025-01-01'::timestamp + interval '144 hours'), (13, 4, 10, -2.25, '', '2025-01-01'::timestamp + interval '22 hours'), (14, 3, 6, 0, 'z', '2025-01-01'::timestamp + interval '147 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 9), (2, 3, 6), (2, 4, 9), (4, 2, 4), (4, 1, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '(7,10]', '{(,7)}', '[2025-01-01,2025-02-01)', '(,0]'), (2, NULL, '{(,3)}', '[2025-03-01,infinity)', '(,0]'), (3, '[6,7)', NULL, 'empty', '[0,0]'), (4, NULL, '{}', NULL, '[0,0]'), (5, '(6,12]', '{(,6)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, ''), (2, 'the quick brown fox'), (3, 'dog-eat-dog world'), (4, 'dog-eat-dog world'), (5, 'a rat ran');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3102) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2319) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2851) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

INSERT INTO r6u (id, email, name, n) VALUES (11, 'm11@x.org', 'deea', 1), (8, 'm8@x.org', 'deeb', 2) ON CONFLICT  DO NOTHING RETURNING *;

CREATE TABLE r9a59260 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a59260 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3231, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3231) i;

ANALYZE (SKIP_LOCKED) r9a59260;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a59260 WHERE g = 1$$);

SELECT * FROM r9est($$SELECT * FROM r9a59260 WHERE g = 3$$);

SELECT * FROM r9est($$SELECT * FROM r9a59260 WHERE tv = 'xxx'$$);

DROP TABLE r9a59260;

CREATE TABLE r9a60687 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a60687 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3195, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3195) i;

CREATE STATISTICS r9a60687_st (mcv) ON g, s FROM r9a60687;

VACUUM ANALYZE r9a60687;

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a60687$$);
