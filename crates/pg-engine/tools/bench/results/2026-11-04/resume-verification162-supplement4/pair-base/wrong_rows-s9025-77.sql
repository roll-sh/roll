-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a20295  (rows=21)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a20295  (rows=1397)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a text[], b int4[], g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t0 (id, a, b) VALUES (2, '{ab}', '{NULL,3}'), (6, '{z}', '{1,2}'), (20, '{a,b}', NULL), (29, '{A,a}', '{NULL,3}'), (26, '{z}', '{1,2}'), (32, NULL, '{NULL,3}'), (4, '{A,a}', NULL), (19, '{ab}', '{5}'), (34, '{ab}', '{1,1,2}'), (10, NULL, '{5}'), (33, '{b,NULL}', '{2}'), (60, '{a,b}', NULL);

CREATE INDEX t0_i756 ON t0 ((g + 1));

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a jsonb, b int8, c text[], d jsonb, e numeric(10,2), g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (4, '[1, 2, 3]', NULL, NULL, NULL, 4.50), (13, '{"a": 1, "b": "x"}', 1, '{A,a}', '{"a": {"b": 2}}', NULL), (27, '{"a": {"b": 2}}', 15, '{z}', '{"a": {"b": 2}}', NULL), (10, NULL, 13, '{ab}', '{"a": 1, "b": "x"}', 6.00), (8, NULL, 11, NULL, NULL, NULL), (22, '{"a": 1, "b": "x"}', NULL, '{}', NULL, 8.25), (40, '1', 0, '{b,NULL}', NULL, NULL), (36, '{"a": 1, "b": "x"}', 20, '{}', NULL, 6.75), (53, '[1, 2, 3]', NULL, NULL, '{"a": {"b": 2}}', NULL), (7, 'null', NULL, '{z}', NULL, 7.75), (19, NULL, NULL, NULL, NULL, 1.00), (6, '{"a": 3}', 17, '{z}', NULL, 6.00), (38, '{"b": [1, "a"]}', 12, '{a,b}', '{"a": 3}', NULL), (26, NULL, NULL, '{A,a}', '[]', 12.00), (28, '1', NULL, '{z}', NULL, NULL), (11, '{"a": "1"}', NULL, '{a,b}', NULL, NULL), (59, '1', 1, NULL, '"s"', 2.05), (42, '{"a": {"b": 2}}', NULL, '{A,a}', 'null', NULL), (46, '1', NULL, NULL, NULL, 0.50), (15, '{"a": 3}', NULL, '{A,a}', 'null', 1.00), (2, '{"a": 1, "b": "x"}', 4, '{a,b}', 'null', 7.50), (29, 'null', NULL, '{}', NULL, NULL), (12, '[]', NULL, '{ab}', '"s"', 0.25), (5, '{"a": 1, "b": "x"}', 0, '{}', NULL, NULL), (35, NULL, NULL, NULL, '"s"', 6.25), (54, '{"a": "1"}', 15, NULL, NULL, 4.75), (23, NULL, 2, '{a,b}', NULL, 12.00), (52, NULL, NULL, '{A,a}', NULL, 5.00), (31, '{"a": "1"}', 14, NULL, NULL, NULL), (21, NULL, -2, NULL, NULL, 12.50);

CREATE INDEX t1_i306 ON t1 (c);

CREATE INDEX t1_i501 ON t1 (b) WHERE b IS NOT NULL;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT CASE WHEN ('{"a": null}'::jsonb ?| ARRAY['a', 'x']) IS NOT FALSE THEN (11::int8)::int4 ELSE NULLIF(GREATEST(p, p), ('{"a": 2.50}'::jsonb::jsonb ->> 'a')::int) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql VOLATILE STRICT AS $$ SELECT CASE WHEN '2024-12-04 15:00:00-05'::timestamptz < ('2025-10-25 14:00:00'::timestamp AT TIME ZONE 'UTC') THEN p ELSE (ceil(99.99::numeric))::int4 END $$;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{}', '{}', '2025-11-02 01:30:00-05', '0001-01-01 00:00:00 BC', NULL, '-1 mon 2 days', '2.5', '-1.25', NULL), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-10-26 00:30:00+00', 'infinity', '2000-03-01', '23:59:59.999999', '-0.0', '0.5', NULL), (3, '"str"', '"str"', '2025-04-06 02:30:00+11', '2000-01-01 00:00:00', '1999-12-31', '-infinity', NULL, '-3', 7), (4, NULL, NULL, '2024-02-29 23:00:00+00', '2024-02-29 00:00:00', '-infinity', '1 day', NULL, '0.125', -3), (5, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2025-11-02 01:30:00', NULL, '2 weeks 3 microseconds', '-7', 'NaN', 3), (6, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2024-02-29 23:00:00+00', '0001-01-01 00:00:00 BC', '-infinity', '0', '0.000001', '1024', -1), (7, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '1999-12-31 23:59:59.999999+00', '2024-02-29 00:00:00', '2025-03-09', 'infinity', '99999999999999999999', '-3', 11), (8, 'null', 'null', NULL, '2000-01-01 00:00:00', '1999-12-31', '1 mon -30 days', 'Infinity', '2', NULL), (9, 'null', 'null', NULL, '0001-01-01 00:00:00 BC', '4713-01-01 BC', '2 weeks 3 microseconds', NULL, '-1.25', 4), (10, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', 'infinity', '-infinity', NULL, '2.5', NULL, 9), (11, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '-infinity', '2025-10-26 02:30:00', '-infinity', '23:59:59.999999', '-1e-20', '2', 9), (12, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', '2025-10-26 02:30:00', '-infinity', NULL, '0', '0.125', NULL), (13, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', 'infinity', '2024-02-29 00:00:00', '4713-01-01 BC', '1 year 2 mons', '123.45', '-0', 10), (14, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2024-02-29 23:00:00+00', '2025-10-26 02:30:00', '2000-03-01', '2 weeks 3 microseconds', '-2.5', '-1.25', 12);

INSERT INTO tp4 VALUES (18, '1999-12-31', 'b', '-0.0'), (21, '2025-03-09', 'b', 'Infinity'), (0, NULL, 'b', '-Infinity'), (8, 'infinity', 'd', '99999999999999999999'), (4, '-infinity', 'ab', '-2.5'), (3, '-infinity', 'd', '-2.5'), (10, '-infinity', 'ab', '1.23456789012345678901234567890'), (8, '4713-01-01 BC', 'a', '1.23456789012345678901234567890'), (-3, '-infinity', 'ab', '0.000001');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "en-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 0, 'Ab', '-Infinity', '0', '[0,10)', '{[2,4)}', 'Fat cats! FAT rats? fat-free', NULL), (2, NULL, 'E', '0', '30 days', '[0,10)', '{(,0],[10,)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(1, ' a')::r5ct, ROW(0, 'I')::r5ct, ROW(NULL, NULL)::r5ct]), (3, 14, NULL, '0.0000000000000000000000000001', '1 year -1 sec', '[0,10)', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', '{}'), (4, 19, ' a', '0.0000000000000000000000000001', '178000000 years', '(,2]', '{(,0],[10,)}', '', ARRAY[ROW(1, 'é')::r5ct, ROW(NULL, 'İ')::r5ct]), (5, 13, 'å', '-99999999999999999999999999999999999.999999', '1 year -1 sec', '[-3,0]', NULL, 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(NULL, '10')::r5ct, ROW(4, 'AB')::r5ct]), (6, 15, '-a', '-Infinity', '720:00:00', '[3,)', NULL, 'Supernovae stars are the brightest phenomena in galaxies', NULL);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 4 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 23 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (16, 'a', '2024-02-29'), (-2, 'b', '2024-02-29'), (7, 'c', '2024-02-29'), (19, 'd', 'infinity'), (-4, 'a', '2025-01-01'), (-2, 'b', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL), (24, 'x1', NULL), (18, 'x2', NULL), (32, 'x3', NULL), (18, 'x4', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 1, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'ann2', 2, '{}', '{"seats": 1.50}'), (3, 'u3@x.org', 'zoë3', 3, '{a}', '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', 'zoë4', 4, '{a,b}', NULL), (5, 'u5@x.org', 'bob5', 0, '{b,NULL}', '[1, {"a": null}]'), (6, 'u6@x.org', 'ann6', 6, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (7, 'u7@x.org', 'dee7', 0, '{}', '{}'), (8, 'u8@x.org', 'ann8', 0, '{b,NULL}', '[1, {"a": null}]'), (9, 'u9@x.org', 'ann9', 0, NULL, '{"plan": "free", "tags": [1, 2]}'), (10, 'u10@x.org', 'ann10', 0, '{}', '[1, {"a": null}]'), (11, 'u11@x.org', 'zoë11', 11, '{b,NULL}', '{}'), (12, 'u12@x.org', 'Éva12', 0, '{b,NULL}', NULL);

INSERT INTO r6o (uid, amount, status, created, data) VALUES (8, 100, 'void', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (8, NULL, 'paid', '2025-02-21', NULL), (6, 9.99, 'new', '2025-02-09', '{"items": [{"sku": "a", "qty": 2}]}'), (12, 9.99, 'void', '2025-02-15', '{"gift": true}'), (4, 9.99, 'paid', '2025-02-03', '{"gift": true}'), (2, 100, 'void', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (11, 12345678.90, 'new', '2025-02-11', '{"gift": true}'), (11, 12345678.90, 'void', '2025-02-03', '{"gift": true}'), (11, -1.50, 'void', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (8, 12345678.90, 'void', '2025-02-24', '{"items": []}'), (4, 0, 'void', '2025-02-28', '{"gift": true}'), (7, 9.99, 'void', '2025-02-07', '{"gift": true}'), (12, 9.99, 'void', '2025-02-15', '{"gift": true}'), (1, 0, 'void', '2025-02-26', NULL);

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

INSERT INTO r7k VALUES (1, 1, 40, 'Note 3', NULL, NULL, '2025-11-02 05:30:00+00'), (2, 6, -35, 'note_4', -12.125, NULL, '2024-02-29 12:00:00+00'), (3, 3, NULL, 'n%5', 3.333, NULL, NULL), (4, 2, 44, 'note_4', -12.125, 2.25, '2025-11-02 05:30:00+00'), (5, 1, 41, 'note 2', NULL, 'Infinity', NULL), (6, 3, 50, 'note 1', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (7, 1, 20, 'note_4', 1000, 'Infinity', NULL), (8, 9, 70, 'note 3', 3.333, -0, NULL), (9, 1, 85, 'NOTE 3', 0.5, 'NaN', '2025-03-09 06:59:59+00'), (10, 1, -1, 'n%5', 3.333, -0, '2025-11-02 05:30:00+00'), (11, 2, 160, 'note 1', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (12, 4, 42, 'NOTE 3', NULL, 2.25, '2025-11-02 05:30:00+00'), (13, 4, 186, 'NOTE 3', 0.5, NULL, '2024-02-29 12:00:00+00'), (14, 4, 29, '', 1000, 1e300, '2024-02-29 12:00:00+00'), (15, 7, 121, 'Note 3', 0.5, NULL, NULL), (16, 1, -35, 'note_4', 1000, 'Infinity', '2025-03-09 07:00:00+00'), (17, 9, -33, 'note 1', 0, NULL, NULL), (18, 1, 138, 'note_4', 0.5, -0, '2025-03-09 07:00:00+00'), (19, 7, 6, 'note_4', -12.125, 'NaN', '2025-03-09 07:00:00+00'), (20, 1, 14, 'note 3', 1000, 0.5, '2025-11-02 05:30:00+00'), (21, 1, -11, 'n%5', 1000, 1e300, NULL), (22, 7, -7, 'n%5', 1000, NULL, '2024-02-29 12:00:00+00'), (23, 1, 171, '', -12.125, 1e300, '2025-11-02 05:30:00+00'), (24, 4, 177, 'n%5', 0, 'Infinity', '2024-02-29 12:00:00+00'), (25, 1, -44, 'NOTE 3', NULL, 'Infinity', '2025-03-09 06:59:59+00'), (26, 1, 157, 'note 2', 0, 'NaN', NULL), (27, 1, 121, 'note 1', NULL, NULL, '2025-03-09 07:00:00+00'), (28, 1, -6, 'note 2', 0.5, 0.5, '2025-11-02 05:30:00+00'), (29, 1, 180, 'NOTE 3', NULL, 0.5, '2025-03-09 07:00:00+00'), (30, 1, 143, 'note 1', 3.333, NULL, '2025-03-09 06:59:59+00'), (31, 1, 27, 'Note 3', 1000, -0, '2025-03-09 06:59:59+00'), (32, 6, NULL, '', -12.125, 'NaN', '2025-03-09 06:59:59+00'), (33, 1, 112, '', NULL, 0.5, '2024-02-29 12:00:00+00'), (34, 1, 10, 'Note 3', 1000, 'NaN', NULL), (35, 1, 127, 'note 3', NULL, 1e300, '2025-03-09 07:00:00+00'), (36, 1, 101, 'note 2', 1000, -0, NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10), (9, 'us', 17), (10, 'eu', 1);

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

INSERT INTO r7t VALUES (1, 'é', 'é'), (2, 'Ä', 'Ä'), (3, 'z', 'z'), (4, 'Æ', 'Æ'), (5, 'ss', 'ss'), (6, 'Ä', 'Ä'), (7, 'Ä', 'Ä'), (8, '9', '9'), (9, 'ä b', 'ä b'), (10, 'Æ', 'Æ'), (11, 'A', 'A'), (12, 'E', 'E'), (13, 'b', 'b');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, NULL, 0, 'B', '2025-01-01'::timestamp + interval '124 hours'), (2, 1, 2, 1.50, NULL, '2025-01-01'::timestamp + interval '29 hours'), (3, 1, 12, 3.75, NULL, '2025-01-01'::timestamp + interval '0 hours'), (4, 3, 15, 10, 'ab', '2025-01-01'::timestamp + interval '52 hours'), (5, 2, 2, 1.50, 'ab', '2025-01-01'::timestamp + interval '69 hours'), (6, 3, 18, 1.50, 'b', '2025-01-01'::timestamp + interval '50 hours'), (7, 2, 16, 0, NULL, '2025-01-01'::timestamp + interval '24 hours'), (8, 2, NULL, 10, 'z', '2025-01-01'::timestamp + interval '46 hours'), (9, 4, 0, 3.75, '', '2025-01-01'::timestamp + interval '141 hours'), (10, 4, 19, NULL, 'a', '2025-01-01'::timestamp + interval '30 hours'), (11, 4, NULL, 0, 'B', '2025-01-01'::timestamp + interval '128 hours'), (12, 2, -5, 10, 'a', '2025-01-01'::timestamp + interval '216 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 6, 3), (3, 2, 2), (3, 6, 5), (4, 5, 5), (4, 4, 6), (6, 6, 2), (6, 2, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', NULL, '(2024-12-31,2025-01-05]', '(,0]'), (2, '[-1,-1)', NULL, NULL, '(,0]'), (3, '[8,14)', '{[8,14), [16, 18)}', 'empty', '[0,0]'), (4, '[5,)', '{}', '[2025-03-01,infinity)', NULL), (5, '[6,)', '{[6,8), [10, 12)}', '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (6, 'empty', '{}', NULL, '[1.5,2.5)'), (7, '(3,5]', '{}', '[2025-01-01,2025-02-01)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'the quick brown fox'), (2, 'foxes and dogs'), (3, 'dog-eat-dog world'), (4, 'foxes and dogs'), (5, 'brown''s fox''s den');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '"scalar"'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3447) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2485) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3743) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r9i11558 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i11558 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3333) i;

CREATE UNIQUE INDEX r9i11558_x0 ON r9i11558 USING btree (a);

CREATE INDEX CONCURRENTLY ON r9i11558 USING btree (t COLLATE "C");

CREATE INDEX r9i11558_x2 ON r9i11558 USING gist (p);

REINDEX TABLE r9i11558;

SET enable_seqscan = off;

SET enable_indexscan = off;

EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i11558 ORDER BY p <-> point '(3,3)', id LIMIT 4;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i11558;

CREATE FUNCTION r9f48284(int, int) RETURNS bigint LANGUAGE sql STABLE AS $f$SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2$f$;

DROP FUNCTION r9f48284;

PREPARE r9p86283 (int, int) AS SELECT id FROM r9s WHERE g = $1 ORDER BY id DESC LIMIT 2 OFFSET $2;

EXECUTE r9p86283(1, 0);

EXPLAIN (COSTS OFF) EXECUTE r9p86283(1, 5);

EXECUTE r9p86283(1, 5);

EXECUTE r9p86283(99, -1);

EXECUTE r9p86283(1, -1);

EXECUTE r9p86283(1, 1000);

EXECUTE r9p86283(1, 5);

EXPLAIN (COSTS OFF) EXECUTE r9p86283(2, 1000);

EXECUTE r9p86283(2, 1000);

EXECUTE r9p86283(99, 99);

EXECUTE r9p86283(3, 99);

EXECUTE r9p86283(5, 99);

EXECUTE r9p86283(1, 5);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p86283';

DEALLOCATE r9p86283;

RESET plan_cache_mode;

\x2 name=r9x4579 describe p=t'8' p=t'777'
SELECT sum(v) FILTER (WHERE g = $1), count(*) FILTER (WHERE v IS NULL) FROM r9s WHERE id <= $2;

\x2 name=r9x4579 rf=1,0 p=b00000001 p=b00000002
-;

\x2 name=r9x4579 rf=1 p=t'1' p=t'1499'
-;

\x2 name=r9x4579 p=b00000001 p=t'100'
-;

\x2 name=r9x4579 rf=1,0 p=t'8' p=t'1499'
-;

\x2 name=r9x4579 rf=1 p=b00000002 p=t'777'
-;

\x2 name=r9x4579 rf=0 p=t'3' p=t'2147483647'
-;

\x2 name=r9x4579 rows=1 p=t'1' p=b00000001
-;

\x2 name=r9x4579 rf=1 p=b00000063 p=b00000309
-;

\x2 name=r9x4579 rf=0 p=t'8' p=t'1499'
-;

\x2 name=r9x4579 rf=0 p=t'5' p=b000005db
-;

\x2 name=r9x4579 rf=1 p=b00000001 p=b7fffffff
-;

\x2 name=r9x4579 p=t'1' p=t'50'
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x4579';

\s2 DEALLOCATE r9x4579;

\s2 RESET plan_cache_mode;

CREATE FUNCTION r9f15470(int, int) RETURNS bigint LANGUAGE sql STABLE AS $f$SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2$f$;

DROP FUNCTION r9f15470;

SET enable_hashagg = off;

RESET enable_hashagg;

ALTER TABLE r6u RENAME COLUMN name TO full_name;

INSERT INTO r6u (id) VALUES (33) ON CONFLICT DO NOTHING;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = auto; FOR i IN 0..13 LOOP s := s + coalesce((SELECT count(*) FROM r9s WHERE g = i % 4 + 1), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

CREATE TABLE r9a20295 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a20295 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4190, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4190) i;

VACUUM ANALYZE r9a20295;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a20295 WHERE g = 3$$);

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a20295 WHERE arr @> '{2}'$$);
