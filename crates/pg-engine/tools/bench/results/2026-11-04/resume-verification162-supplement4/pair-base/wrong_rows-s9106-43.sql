-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a24703  (rows=8)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a24703  (rows=554)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a date, b int4[], c int8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

CREATE TABLE t0_c (extra int) INHERITS (t0);

INSERT INTO t0 (id, a, b, c) VALUES (17, '2024-03-05', NULL, NULL), (27, NULL, '{1,2}', NULL), (18, NULL, NULL, NULL), (15, NULL, NULL, NULL), (19, NULL, '{}', NULL), (34, '2024-02-04', NULL, NULL), (1, '2024-03-23', NULL, NULL), (2, NULL, '{2}', NULL), (36, NULL, NULL, NULL), (30, '2024-02-07', NULL, NULL), (40, '2024-02-18', NULL, 9), (14, NULL, '{5}', NULL), (28, '2024-02-22', '{1,1,2}', NULL), (33, NULL, NULL, 4), (11, '2024-01-22', '{3,1,2}', NULL), (53, NULL, NULL, 4), (46, NULL, NULL, NULL), (47, '2024-03-17', NULL, NULL), (29, NULL, NULL, NULL), (7, NULL, NULL, NULL), (6, '2024-03-25', '{1,2}', NULL), (8, NULL, NULL, NULL), (3, NULL, NULL, NULL), (35, NULL, '{1,1,2}', NULL), (20, '2024-03-23', NULL, NULL), (25, '2024-01-01', NULL, NULL), (44, NULL, '{NULL,3}', 11), (50, NULL, NULL, NULL), (55, NULL, '{}', 11), (31, NULL, NULL, NULL);

CREATE INDEX t0_i983 ON t0 USING hash (b);

CREATE INDEX t0_i427 ON t0 (g, id);

CREATE INDEX t0_i594 ON t0 USING hash (b);

CREATE TABLE t1 (id int4 PRIMARY KEY, a int8, b date, c numeric(10,2), d int2, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t1 (id, a, b, c, d) VALUES (12, -1, '2024-03-13', 2.25, 2), (29, 0, '2024-01-28', NULL, 2), (25, 8, '2024-01-26', 4.00, 0), (1, 3, '2024-03-22', NULL, NULL), (57, NULL, '2024-02-17', 4.25, NULL), (35, 12, NULL, 10.00, 2), (24, 11, '2024-01-24', NULL, 6), (31, 0, '2024-02-27', 3.05, NULL), (40, NULL, '2024-02-28', 7.75, 5), (15, NULL, NULL, NULL, NULL), (27, 14, NULL, -2.50, 5), (17, NULL, NULL, -2.75, 6);

ANALYZE;

CREATE VIEW v0 AS SELECT '1 mon -1 day'::interval = '0'::interval AS c0, (SELECT every(false) FROM t0 t2 WHERE false) AS c1 FROM t1 t1 WHERE NOT ((int4range(CASE WHEN true THEN 1 ELSE t1.g END, CASE WHEN false THEN t1.id ELSE t1.id END, '[]') -|- int4range(t1.g, t1.g, '()')));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-10-26 00:30:00+00', '0001-01-01 00:00:00 BC', '2025-01-01', 'infinity', '0.000001', '0.125', 5), (2, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '0001-01-01 00:00:00 BC', '2025-01-01', '1 day', '-7', NULL, 5), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', '2000-01-01 00:00:00', '1999-12-31', '1 year 2 mons', '-7', '0.5', NULL), (4, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', '2025-12-31 23:59:59.999999', '2025-03-09', '2 weeks 3 microseconds', '-2.5', '0.125', 7), (5, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2025-11-02 01:30:00-04', '2025-11-02 01:30:00', '2024-02-29', '-1 mon 2 days', '2.5', 'Infinity', 1), (6, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '2025-12-31 23:59:59.999999', '2025-01-01', '2 weeks 3 microseconds', '99999999999999999999', '2', -3), (7, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2024-02-29 00:00:00', '2024-02-29', '1 day', '-1e-20', 'NaN', 12), (8, NULL, NULL, '2025-10-26 00:30:00+00', '1970-01-01 00:00:00.5', '-infinity', '1 year 2 mons', NULL, '0.125', 10), (9, NULL, NULL, '2038-01-19 03:14:08+00', 'infinity', NULL, '-1 year -1 day', '99999999999999999999', '0.5', 12), (10, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', 'infinity', '1999-12-31', '1 day', '-2.5', 'Infinity', NULL), (11, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', '2025-03-09 02:30:00', '1999-12-31', '-infinity', '-0.0', '1024', 11), (12, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', NULL, NULL, '23:59:59.999999', 'NaN', NULL, -3), (13, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2025-03-09 01:30:00-05', NULL, '2025-03-09', 'infinity', '0.000001', 'Infinity', 2), (14, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '0044-03-15 12:00:00+00 BC', '2025-11-02 01:30:00', '-infinity', '-1 mon 2 days', '-0.0', '2', -1);

INSERT INTO tp4 VALUES (6, 'infinity', NULL, '1e30'), (2, '1999-12-31', 'a', '-7'), (-4, '2024-02-29', NULL, NULL), (20, '-infinity', 'c', '2.5'), (20, 'infinity', NULL, '0.000001'), (-4, '2025-01-01', 'a', '0.000001'), (22, '2024-02-29', NULL, '0.000001');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5ai", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 9, 'ı', '0.0000000000000000000000000001', '-1 mon 30 days', '(,)', '{[1,3),[5,7)}', 'the and of', '{}'), (2, 17, 'ı', 'Infinity', '2147483647 mons', NULL, '{[1,2],[2,3)}', NULL, '{}'), (3, -3, '10', '-Infinity', 'infinity', '[-3,0]', '{}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(NULL, 'a-')::r5ct, ROW(-2, 'b')::r5ct]), (4, 15, '-a', '-Infinity', 'infinity', 'empty', '{[1,2],[2,3)}', '', ARRAY[ROW(NULL, 'Z')::r5ct, ROW(-1, 'a-')::r5ct]), (5, 5, 'A', '-0.5', '2147483647 days', '[3,)', '{[1,3),[5,7)}', NULL, NULL), (6, 1, 'o', NULL, '1 day 00:00:00.000001', '[0,10)', '{[0,100)}', '', ARRAY[ROW(4, '-a')::r5ct]), (7, 12, '-a', '0.0000000000000000000000000001', '2147483647 days', '[-3,0]', '{[1,2],[2,3)}', 'running runners ran; the runner runs', '{}');

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_b ON r5a (b COLLATE "tr-x-icu");

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (NEW.g1 > 0) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 22 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (6, 'a', NULL), (-5, 'b', 'infinity'), (2, 'c', 'infinity'), (15, 'd', '2025-01-01'), (-5, 'a', 'infinity'), (-5, 'b', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (31, 'x0', NULL), (25, 'x1', NULL), (29, 'x2', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob1', 1, '{a}', '[1, {"a": null}]'), (2, 'u2@x.org', '2', 0, '{a}', '{"seats": 1.50}'), (3, 'u3@x.org', 'zoë3', 3, '{a}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', 'ann4', 4, '{a,b}', '[1, {"a": null}]'), (5, 'u5@x.org', '5', 0, '{b,NULL}', '{}'), (6, 'u6@x.org', 'zoë6', 6, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'bob 7', 7, '{b,NULL}', NULL), (8, 'u8@x.org', 'bob 8', 0, NULL, '[1, {"a": null}]'), (9, 'u9@x.org', 'bob 9', 9, '{a}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (8, 12345678.90, 'new', '2025-02-13', NULL), (1, 9.99, 'new', '2025-02-25', '{"items": []}'), (9, 9.99, 'paid', '2025-02-22', '{"gift": true}'), (9, 12345678.90, 'void', '2025-02-02', '{"items": []}'), (5, -1.50, 'new', '2025-02-15', '{"items": []}'), (3, 12345678.90, 'new', '2025-02-24', '{"items": [{"sku": "a", "qty": 2}]}'), (7, 100, 'void', '2025-02-24', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 84, 'note_4', 0.5, 0.5, '2025-11-02 05:30:00+00'), (2, 2, 38, 'é', 0, -0, '2025-03-09 06:59:59+00'), (3, 1, 82, 'n%5', 1000, -0, '2025-03-09 07:00:00+00'), (4, 5, 26, 'note 1', 1000, 1e300, '2025-03-09 07:00:00+00'), (5, 1, 82, 'note 3', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (6, 1, 171, 'note_4', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (7, 7, 155, 'NOTE 3', 1000, 'NaN', '2024-02-29 12:00:00+00'), (8, 5, 174, 'note 3', 0, 'Infinity', '2025-03-09 06:59:59+00'), (9, 1, 38, 'note_4', 3.333, 2.25, '2025-11-02 05:30:00+00'), (10, 1, 196, 'note 2', -12.125, 'Infinity', '2025-03-09 07:00:00+00'), (11, 1, -3, 'é', 0, 'Infinity', '2025-03-09 06:59:59+00'), (12, 1, 107, 'note 2', 1000, 1e300, '2025-03-09 06:59:59+00'), (13, 1, 110, 'NOTE 3', -12.125, 1e300, '2025-03-09 06:59:59+00'), (14, 1, 178, 'Note 3', 0.5, -0, '2025-03-09 06:59:59+00'), (15, 6, 175, 'note 3', 0.5, NULL, '2025-03-09 06:59:59+00'), (16, 1, 190, 'note 2', -12.125, NULL, '2025-03-09 07:00:00+00'), (17, 1, NULL, 'NOTE 3', 0, 0.5, '2025-03-09 06:59:59+00'), (18, 1, 42, 'note 3', 0.5, 'NaN', NULL), (19, 1, 159, 'Note 3', 1000, 'Infinity', '2025-03-09 06:59:59+00'), (20, 1, -26, 'note 1', 1000, -0, '2025-03-09 07:00:00+00'), (21, 8, 107, 'Note 3', 0.5, 0.5, '2025-03-09 06:59:59+00'), (22, 1, 15, 'n%5', 3.333, 1e300, NULL), (23, 9, 93, 'note_4', 0, 'Infinity', '2024-02-29 12:00:00+00'), (24, 1, 189, 'note_4', 1000, 0.5, '2025-03-09 07:00:00+00'), (25, 1, 181, 'note 3', -12.125, -0, '2025-03-09 07:00:00+00'), (26, 1, -1, 'note 3', 0.5, 2.25, NULL), (27, 1, 5, 'note 2', -12.125, 2.25, '2025-03-09 06:59:59+00'), (28, 1, 106, 'n%5', 3.333, -0, NULL), (29, 7, 74, 'é', 1000, 1e300, '2025-03-09 06:59:59+00'), (30, 2, 102, 'note 1', 3.333, NULL, '2025-03-09 07:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

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

INSERT INTO r7t VALUES (1, 'Ä', 'Ä'), (2, 'ss', 'ss'), (3, 'b', 'b'), (4, 'e', 'e'), (5, 'z', 'z'), (6, '10', '10'), (7, 'E', 'E'), (8, 'e', 'e');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

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

INSERT INTO r8a VALUES (1, 3, -4, 3.75, 'B', '2025-01-01'::timestamp + interval '102 hours'), (2, 2, NULL, 0, 'ab', '2025-01-01'::timestamp + interval '2 hours'), (3, 1, 15, 3.75, 'b', '2025-01-01'::timestamp + interval '171 hours'), (4, 2, 2, 1.50, NULL, '2025-01-01'::timestamp + interval '22 hours'), (5, 4, 13, 3.75, 'a', '2025-01-01'::timestamp + interval '72 hours'), (6, 4, 17, NULL, NULL, '2025-01-01'::timestamp + interval '171 hours'), (7, 1, 1, 10, 'z', '2025-01-01'::timestamp + interval '86 hours'), (8, 3, 1, 10, 'b', '2025-01-01'::timestamp + interval '45 hours'), (9, 2, 16, 3.75, 'a', '2025-01-01'::timestamp + interval '26 hours'), (10, 1, 16, 3.75, 'B', '2025-01-01'::timestamp + interval '106 hours'), (11, 4, 16, 0, NULL, '2025-01-01'::timestamp + interval '27 hours'), (12, 1, 12, 0, 'ab', '2025-01-01'::timestamp + interval '3 hours'), (13, 3, 13, 3.75, '', '2025-01-01'::timestamp + interval '72 hours'), (14, 3, 2, 1.50, 'z', '2025-01-01'::timestamp + interval '57 hours'), (15, 1, 6, 3.75, NULL, '2025-01-01'::timestamp + interval '75 hours'), (16, 4, -2, NULL, NULL, '2025-01-01'::timestamp + interval '47 hours'), (17, 3, 15, -2.25, 'b', '2025-01-01'::timestamp + interval '177 hours'), (18, 4, 17, NULL, 'ab', '2025-01-01'::timestamp + interval '126 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (3, 3, 9), (5, 5, 9), (5, 2, 8), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{(,10)}', NULL, '(1.25,1.5)'), (2, NULL, NULL, NULL, '(1.25,1.5)'), (3, '[7,12)', '{[7,12), [14, 16)}', NULL, NULL), (4, '[-3,)', '{(,-3)}', '[2025-01-01,2025-02-01)', '[0,0]'), (5, 'empty', '{[4,8), [10, 12)}', '[2025-03-01,infinity)', '(,0]'), (6, '(-1,2]', '{(,-1)}', 'empty', '[1.5,2.5)'), (7, '[9,15)', '{(,9)}', 'empty', '(1.25,1.5)'), (8, '[4,9)', '{[4,9), [11, 13)}', NULL, '(,0]'), (9, '[9,10)', '{[9,10), [12, 14)}', '[2025-01-01,2025-02-01)', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'brown''s fox''s den'), (2, 'dog-eat-dog world'), (3, 'dog-eat-dog world'), (4, 'quick quick slow'), (5, 'jumps over the lazy dog'), (6, 'quick quick slow'), (7, 'dog-eat-dog world');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3725) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1808) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2741) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

\s2 BEGIN;

\s2 UPDATE r8a SET v = coalesce(v, 0) + 100 WHERE id = 3 RETURNING id, v;

\s2 INSERT INTO r8log (tg, info) VALUES ('2pc', 'r8gx84478');

\s2 PREPARE TRANSACTION 'r8gx84478';

SELECT gid, owner, database = current_database() AS here FROM pg_prepared_xacts ORDER BY gid;

\s2 PREPARE TRANSACTION 'r8gx84478';

\s3& UPDATE r8a SET v = -1 WHERE id = 3 RETURNING id, v;

SELECT 1;

ROLLBACK;

\s2 ROLLBACK;

COMMIT PREPARED 'r8gx84478';

\s3.;

COMMIT PREPARED 'r8gx84478';

SELECT count(*) FROM pg_prepared_xacts WHERE gid = 'r8gx84478';

CREATE TABLE r9i15630 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i15630 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 2999) i;

ANALYZE r9i15630;

CREATE INDEX r9i15630_x0 ON r9i15630 USING brin (a) WITH (pages_per_range = 2);

CREATE INDEX ON r9i15630 USING gist (r) WHERE NOT isempty(r);

ALTER TABLE r9i15630 ADD CONSTRAINT r9i15630_ex EXCLUDE USING gist (r WITH &&);

SET enable_seqscan = on;

SET enable_indexonlyscan = off;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i15630;

CREATE TABLE r9a24703 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a24703 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1662, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1662) i;

CREATE STATISTICS r9a24703_st ON g, s FROM r9a24703;

VACUUM ANALYZE r9a24703;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a24703 WHERE tv = 'xxx'$$);

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a24703 WHERE arr @> '{2}'$$);
