-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: r9est:25
-- engine: HashAggregate  (rows=1800)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a57078  (rows=1804)
-- engine: -- SELECT 3
-- reference: r9est:25
-- reference: HashAggregate  (rows=1804)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a57078  (rows=1804)
-- reference: -- SELECT 3

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a text COLLATE "ci3", b int4, c d3text);

CREATE TABLE t0_c (extra int) INHERITS (t0);

INSERT INTO t0_c (id, a, b, c, extra) VALUES (635, NULL, NULL, NULL, 1);

INSERT INTO t0_c (id, a, b, c, extra) VALUES (835, NULL, NULL, NULL, 3);

INSERT INTO t0_c (id, a, b, c, extra) VALUES (332, NULL, 8, NULL, 1);

INSERT INTO t0 (id, a, b, c) VALUES (32, NULL, NULL, 'Z'), (5, '10', 6, 'z'), (54, 'Ä', NULL, NULL), (38, NULL, 1, 'a_b'), (10, '10', 5, NULL), (8, 'ab', 4, NULL), (18, 'A', NULL, NULL), (27, 'a', NULL, NULL), (12, 'a%', 6, NULL), (2, NULL, 6, 'Ä'), (25, NULL, -1, NULL), (31, 'Ä', NULL, 'a,b');

CREATE INDEX t0_i746 ON t0 (id);

CREATE INDEX t0_i749 ON t0 (id);

CREATE TABLE t1 (id int4 PRIMARY KEY, a int4 DEFAULT -1, b interval, c d3text, d int4);

INSERT INTO t1 (id, a, b, c, d) VALUES (32, 1, '2 hours', 'b', NULL), (33, 4, '1 mon', 'z', 3), (36, -1, NULL, 'a', NULL), (14, -1, NULL, NULL, NULL), (3, 9, NULL, 'ß', 3), (10, 0, '1 mon', 'ba', 8), (6, NULL, '1 day', 'bar', 6), (35, NULL, '30 days', 'x ', 3), (17, 0, '1 day', 'é', 9), (9, 4, NULL, 'a_b', 1), (24, 6, '30 days', ' x', 2), (39, 6, '30 days', 'bar', 1), (21, -2, '2 hours', NULL, NULL), (4, 5, '0', 'a', 7), (12, 0, NULL, 'x ', NULL), (31, 6, '24:00:00', 'ß', 4), (48, 1, '30 days', '10', 1), (38, -1, '-1 day', 'A', 7), (16, 7, '0', 'B', 5), (58, NULL, '0', 'a%', 7);

CREATE INDEX t1_i40 ON t1 ((id + 1));

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; NEW.a := coalesce(NEW.a, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER DELETE ON t1 REFERENCING OLD TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t2 (id int4 PRIMARY KEY, a json, b text[], c d3text, d int8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t2 (id, a, b, c, d) VALUES (14, '1', NULL, 'ba', NULL), (9, '{"a": "1"}', '{A,a}', ' x', 14), (13, NULL, '{z}', NULL, 10);

CREATE TABLE t3 (id int4 PRIMARY KEY, a date, b json);

INSERT INTO t3 (id, a, b) VALUES (9, NULL, NULL), (20, '2024-03-26', '1'), (5, '2024-03-15', '[1, 2, 3]');

CREATE INDEX t3_i526 ON t3 (id) WHERE id IS NOT NULL;

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW(NULL, -1)::ct3, NULL, 0), (2, ROW('a_b', NULL)::ct3, '{}', 3), (3, ROW('ß', 1)::ct3, ARRAY[ROW('aa', 1)::ct3, ROW(NULL, 0)::ct3], 4), (4, ROW('é', -1)::ct3, ARRAY[ROW('a%', 2)::ct3, ROW('aa', 3)::ct3], 0), (5, ROW('ba', 2)::ct3, '{}', NULL);

CREATE INDEX tc3_c ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-04-06 02:30:00+11', '2025-12-31 23:59:59.999999', '2024-02-29', NULL, '-1e-20', NULL, -2), (2, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-03-09 01:30:00-05', '2025-03-09 02:30:00', '1999-12-31', '-infinity', 'NaN', '-3', -1), (3, 'null', 'null', '2025-03-09 03:30:00-04', '2025-03-09 02:30:00', NULL, NULL, '-Infinity', '0.5', 12), (4, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', NULL, '-infinity', '1 mon -30 days', '-7', '-0', 6), (5, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2024-02-29 23:00:00+00', '2024-02-29 00:00:00', '2024-02-29', '2 weeks 3 microseconds', '-Infinity', '0.5', 5), (6, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '-infinity', '2025-12-31 23:59:59.999999', '2025-01-01', '23:59:59.999999', '123.45', 'NaN', 12), (7, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-03-09 01:30:00-05', '2025-03-09 02:30:00', '2025-01-01', '2 weeks 3 microseconds', NULL, 'Infinity', 1);

INSERT INTO tp4 VALUES (-3, '2000-03-01', NULL, '-1e-20'), (1, '2025-03-09', 'd', '0.5'), (12, '2025-01-01', 'ab', NULL), (21, '4713-01-01 BC', 'b', '0'), (17, '4713-01-01 BC', 'd', '-Infinity'), (-4, '2025-03-09', 'd', '99999999999999999999'), (17, '1999-12-31', 'b', NULL), (7, '2025-03-09', 'a', '1e30'), (9, 'infinity', 'b', NULL), (7, '2025-03-09', 'ab', '0'), (11, 'infinity', NULL, '0.5'), (20, 'infinity', 'a', '123.45'), (23, '4713-01-01 BC', 'd', 'Infinity'), (22, '2025-03-09', 'ab', '-2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "pg_c_utf8", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 3, 'h', '0.0000000000000000000000000001', '1 year -1 sec', '[1,5)', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', '{}'), (2, 7, 'a-', '123456789012345678901234567890.123456789', '1 year -1 sec', '(,)', '{[0,100)}', 'running runners ran; the runner runs', ARRAY[ROW(1, 'ı')::r5ct, ROW(2, NULL)::r5ct]), (3, 1, '', '-0.5', '-1 mon 30 days', NULL, '{[0,100)}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(-2, 'SS')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 19 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (10, 'a', '2024-02-29'), (14, 'b', '2024-02-29'), (17, 'c', 'infinity'), (12, 'd', NULL), (10, 'a', '2024-02-29'), (5, 'b', NULL), (4, 'c', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (31, 'x0', NULL), (20, 'x1', NULL), (23, 'x2', NULL), (26, 'x3', NULL), (23, 'x4', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 1, '{a}', '{"seats": 1.50}'), (2, 'u2@x.org', '2', 2, '{a,b}', '{"seats": 1.50}'), (3, 'u3@x.org', 'ann3', 0, '{}', NULL), (4, 'u4@x.org', 'bob4', 4, '{}', '{"plan": "free", "tags": [1, 2]}'), (5, 'u5@x.org', 'dee5', 5, '{}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, -1.50, 'void', '2025-02-08', '{"gift": true}'), (1, 0, 'void', '2025-02-04', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 9.99, 'new', '2025-02-17', NULL), (2, NULL, 'new', '2025-02-12', NULL), (5, 12345678.90, 'new', '2025-02-26', NULL), (4, NULL, 'new', '2025-02-11', '{"items": []}'), (1, NULL, 'new', '2025-02-16', '{"items": []}'), (3, -1.50, 'paid', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 12345678.90, 'void', '2025-02-03', '{"gift": true}'), (5, 0, 'new', '2025-02-21', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 9.99, 'new', '2025-02-08', NULL), (1, 12345678.90, 'new', '2025-02-08', '{"items": []}'), (4, -1.50, 'paid', '2025-02-03', '{"items": []}'), (4, 100, 'paid', '2025-02-18', '{"items": []}'), (2, 12345678.90, 'paid', '2025-02-26', NULL), (2, 12345678.90, 'void', '2025-02-09', '{"items": [{"sku": "a", "qty": 2}]}');

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

INSERT INTO r7k VALUES (1, 3, NULL, 'Note 3', 3.333, 2.25, '2024-02-29 12:00:00+00'), (2, 1, -35, 'note 3', 0, NULL, '2025-03-09 06:59:59+00'), (3, 1, 65, 'note 2', 3.333, 'Infinity', '2025-03-09 07:00:00+00'), (4, 8, 160, '', 0, 'Infinity', '2025-03-09 07:00:00+00'), (5, 1, 112, 'note 3', 1000, 2.25, '2025-03-09 06:59:59+00'), (6, 5, 159, '', NULL, 0.5, '2025-03-09 06:59:59+00'), (7, 1, 191, 'note 1', 0, 1e300, '2024-02-29 12:00:00+00'), (8, 1, -43, '', 0.5, -0, NULL), (9, 1, -23, 'n%5', NULL, NULL, '2025-03-09 06:59:59+00'), (10, 3, 164, 'NOTE 3', 0.5, NULL, '2025-03-09 06:59:59+00'), (11, 1, 99, 'é', -12.125, 1e300, '2025-03-09 07:00:00+00'), (12, 2, 111, 'n%5', 0, 'NaN', '2025-03-09 07:00:00+00'), (13, 7, 49, 'note 2', -12.125, 'Infinity', NULL), (14, 4, 57, 'Note 3', 3.333, 2.25, '2025-03-09 07:00:00+00'), (15, 6, 1, 'note 3', 3.333, 0.5, '2025-03-09 06:59:59+00'), (16, 1, -17, '', 3.333, -0, '2025-03-09 06:59:59+00'), (17, 1, 86, 'é', 0, 1e300, '2025-03-09 07:00:00+00'), (18, 1, 190, 'note 3', 3.333, 2.25, '2024-02-29 12:00:00+00'), (19, 1, NULL, 'n%5', 0, 'NaN', NULL), (20, 1, 129, 'n%5', 1000, NULL, '2025-03-09 07:00:00+00'), (21, 9, NULL, 'NOTE 3', 0, 1e300, NULL);

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

CREATE TABLE r7g (id int GENERATED BY DEFAULT AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, 'ss', 'ss'), (2, 'ss', 'ss'), (3, 'ae', 'ae'), (4, 'z', 'z'), (5, 'b', 'b'), (6, 'ss', 'ss'), (7, 'b', 'b');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, -2, 0, 'ab', '2025-01-01'::timestamp + interval '159 hours'), (2, 2, 6, 10, 'ab', '2025-01-01'::timestamp + interval '48 hours'), (3, 2, 7, 1.50, 'ab', '2025-01-01'::timestamp + interval '141 hours'), (4, 3, 0, 0, 'b', '2025-01-01'::timestamp + interval '39 hours'), (5, 1, 17, 1.50, NULL, '2025-01-01'::timestamp + interval '80 hours'), (6, 3, -1, 10, 'z', '2025-01-01'::timestamp + interval '10 hours'), (7, 3, 19, -2.25, NULL, '2025-01-01'::timestamp + interval '138 hours'), (8, 4, 12, 0, 'b', '2025-01-01'::timestamp + interval '28 hours'), (9, 3, 12, 10, '', '2025-01-01'::timestamp + interval '56 hours'), (10, 3, 9, 3.75, 'b', '2025-01-01'::timestamp + interval '68 hours'), (11, 2, 4, NULL, 'ab', '2025-01-01'::timestamp + interval '24 hours'), (12, 4, 15, 3.75, 'z', '2025-01-01'::timestamp + interval '192 hours'), (13, 3, NULL, 1.50, '', '2025-01-01'::timestamp + interval '30 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 2, 2), (2, 1, 1), (3, 5, 5), (3, 5, 7), (4, 1, 1), (4, 1, 1), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-2,4)', '{(,-2)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (2, '(3,8]', '{}', 'empty', NULL), (3, NULL, '{}', '[2025-03-01,infinity)', '[1.5,2.5)'), (4, 'empty', NULL, '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (5, '[-1,1)', '{}', '[2025-01-01,2025-02-01)', '(,0]'), (6, '[-3,1)', NULL, 'empty', '[0,0]'), (7, NULL, '{}', '[2025-01-01,2025-02-01)', NULL), (8, '[7,12)', '{(,7)}', '(2024-12-31,2025-01-05]', '(,0]'), (9, NULL, '{(,0)}', NULL, '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, ''), (2, 'foxes and dogs'), (3, 'quick quick slow'), (4, ''), (5, 'running rats'), (6, 'a rat ran'), (7, 'brown''s fox''s den'), (8, '');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (5, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (6, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2882) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1582) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3732) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a12635 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a12635 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4513, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4513) i;

ALTER TABLE r9a12635 ALTER COLUMN u SET (n_distinct = 50);

ANALYZE r9a12635;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a12635 WHERE nul > 100$$);

SELECT * FROM r9est($$SELECT * FROM r9a12635 WHERE g = 3$$);

SELECT * FROM r9est($$SELECT * FROM r9a12635 WHERE g = 3$$);

EXPLAIN SELECT * FROM r9a12635 WHERE g = 3;

DROP TABLE r9a12635;

CREATE TABLE r9a85141 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a85141 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3719, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3719) i;

ALTER TABLE r9a85141 ALTER COLUMN arr SET STATISTICS 0;

ANALYZE r9a85141;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a85141 WHERE u < 100$$);

SELECT * FROM r9est($$SELECT * FROM r9a85141 WHERE s LIKE 'v1%'$$);

DROP TABLE r9a85141;

CREATE TABLE r9a57078 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a57078 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1804, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1804) i;

SET default_statistics_target = 7;

UPDATE r9a57078 SET g = 5, nul = NULL WHERE id % 50 = 0;

VACUUM ANALYZE r9a57078;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a57078 WHERE s LIKE 'v1%'$$);

SELECT * FROM r9est($$SELECT * FROM r9a57078 WHERE u = 5$$);

EXPLAIN SELECT * FROM r9a57078 WHERE u = 5;

-- statement
SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a57078$$);
