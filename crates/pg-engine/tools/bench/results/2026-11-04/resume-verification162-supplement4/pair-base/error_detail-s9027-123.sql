-- class: error_detail
-- ordered: false
-- note: 23P01
-- engine: ERROR 23P01: could not create exclusion constraint "r9i16783_ex"
-- engine: DETAIL: Key (r)=([1,2)) conflicts with key (r)=([0,5)).
-- reference: ERROR 23P01: could not create exclusion constraint "r9i16783_ex"
-- reference: DETAIL: Key (r)=([1,2)) conflicts with key (r)=([0,2)).

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a text, b text, c int4[], d text COLLATE "ci3", e interval);

CREATE TABLE t0_c (extra int) INHERITS (t0);

CREATE INDEX t0_i372 ON t0 USING hash (b);

CREATE INDEX t0_i825 ON t0 (e);

CREATE INDEX t0_i776 ON t0 USING gin (c);

CREATE TABLE t1 (id int4 PRIMARY KEY, a text COLLATE "ci3", b interval);

INSERT INTO t1 (id, a, b) VALUES (20, 'A', '2 hours'), (39, 'abc', NULL), (44, 'b', '1 day'), (10, NULL, '24:00:00'), (32, 'Ab', '-1 day'), (12, 'b', '1 day'), (37, 'x ', '30 days'), (27, 'a%', '24:00:00'), (13, 'a%', '1 mon'), (30, 'ab', '1 mon'), (58, ' x', '0'), (34, NULL, '0');

CREATE INDEX t1_i608 ON t1 (b);

CREATE INDEX t1_i849 ON t1 USING hash (a);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t2 (a bool, b bool, c int4, g int GENERATED ALWAYS AS (coalesce(c, 0) * 2 + 1) VIRTUAL);

INSERT INTO t2 (a, b, c) VALUES (NULL, false, NULL), (false, false, 3), (NULL, false, NULL), (NULL, true, 7), (false, false, -2), (false, true, 2), (NULL, true, NULL), (NULL, NULL, NULL), (false, false, NULL), (NULL, false, NULL), (NULL, false, NULL), (NULL, NULL, NULL), (false, false, 5), (true, NULL, 3), (NULL, NULL, 2), (NULL, false, 6), (NULL, NULL, 4), (false, false, 9), (false, false, NULL), (false, false, NULL), (NULL, NULL, 7), (false, NULL, NULL), (false, NULL, NULL), (false, false, 0), (false, false, NULL), (false, NULL, 3), (NULL, true, NULL), (NULL, NULL, 6), (false, true, -2), (true, NULL, NULL);

CREATE INDEX t2_i420 ON t2 (a);

CREATE INDEX t2_i157 ON t2 (g);

ANALYZE;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, a FROM t1 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, a FROM t1 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT (jsonb_array_length(jsonb_path_query_array(jsonb_build_object('k', (s2.x % 4), 'v', concat_ws('-', s2.y, s2.y)), '$.*'))) AS c0 FROM (t2 t1 FULL JOIN srf0((t1.c - t1.g)) s2 ON t1.c = s2.x);

CREATE VIEW v1 AS SELECT t2.b AS c0 FROM (t2 t1 LEFT JOIN t1 t2 ON t1.c = t2.id);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2024-02-29 23:00:00+00', '2025-10-26 02:30:00', '-infinity', '-1 year -1 day', '-Infinity', 'NaN', -2), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '2025-12-31 23:59:59.999999', NULL, '1 mon -30 days', '2.5', '1024', 11), (3, '"str"', '"str"', '0044-03-15 12:00:00+00 BC', '0001-01-01 00:00:00 BC', '4713-01-01 BC', '2 weeks 3 microseconds', '1.23456789012345678901234567890', '-3', NULL), (4, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '0044-03-15 12:00:00+00 BC', '0001-01-01 00:00:00 BC', '2024-02-29', '23:59:59.999999', '2.5', '0.125', -2), (5, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-10-26 00:30:00+00', '2025-03-09 02:30:00', NULL, '-1 year -1 day', 'NaN', NULL, 9), (6, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-10-26 00:30:00+00', NULL, '1999-12-31', '1 year 2 mons', 'Infinity', 'Infinity', 0);

INSERT INTO tp4 VALUES (18, '2000-03-01', 'ab', '1.23456789012345678901234567890'), (-4, '-infinity', 'a', NULL), (1, '2025-03-09', NULL, '-2.5'), (10, 'infinity', NULL, '-0.0'), (2, '2024-02-29', 'd', '123.45'), (20, '2025-03-09', 'd', '-Infinity'), (22, '4713-01-01 BC', 'b', 'NaN'), (11, NULL, 'd', '-Infinity'), (-3, '2000-03-01', 'ab', '1.23456789012345678901234567890'), (7, '4713-01-01 BC', 'a', '-1e-20'), (0, 'infinity', 'ab', 'Infinity'), (12, '1999-12-31', 'b', '-Infinity'), (19, '2025-01-01', 'ab', '1.23456789012345678901234567890');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "en-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 15, 'o', '-Infinity', '1 mon', '[3,)', '{empty}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', '{}'), (2, NULL, 'ß', '1e-130', '30 days', NULL, '{[1,3),[5,7)}', 'the and of', ARRAY[ROW(-1, '')::r5ct, ROW(4, NULL)::r5ct]), (3, 12, 'SS', '-1e40', '0', '[-3,0]', '{[2,4)}', 'PostgreSQL full text search: lexemes & weights', '{}'), (4, 8, 'ae', '123456789012345678901234567890.123456789', '720:00:00', '[0,10)', '{[2,4)}', 'the and of', '{}'), (5, 20, 'İ', 'NaN', '-178000000 years', '[0,10)', '{[2,4)}', NULL, ARRAY[ROW(3, 'ä')::r5ct, ROW(4, '')::r5ct]), (6, -2, 'ae', '1e100', '-infinity', NULL, '{empty}', '', ARRAY[ROW(NULL, NULL)::r5ct, ROW(NULL, 'h')::r5ct]), (7, 9, 'a', '7', '2147483647 mons', '[3,)', '{[2,4)}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (8, 6, 'AB', '0', '1 day 00:00:00.000001', '[3,)', '{[1,3),[5,7)}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(5, NULL)::r5ct, ROW(0, 'ab')::r5ct]);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -1 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 11 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (11, 'a', '2024-02-29'), (-4, 'b', '2024-02-29') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (31, 'x0', NULL), (31, 'x1', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Cid1', 1, NULL, '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'Éva2', 2, '{a,b}', '{}'), (3, 'u3@x.org', 'zoë3', 3, NULL, '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', 'ann4', 4, NULL, '{}'), (5, 'u5@x.org', 'bob 5', 5, NULL, '{"plan": "pro", "seats": 3}'), (6, 'u6@x.org', 'Cid6', 6, '{}', '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'dee7', 7, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (8, 'u8@x.org', 'ann8', 8, '{a}', NULL), (9, 'u9@x.org', 'ann9', 0, '{a,b}', '{"plan": "free", "tags": [1, 2]}'), (10, 'u10@x.org', 'Éva10', 0, '{a,b}', '{"plan": "pro", "seats": 3}'), (11, 'u11@x.org', 'ann11', 11, '{}', NULL), (12, 'u12@x.org', 'ann12', 0, '{a,b}', NULL);

INSERT INTO r6o (uid, amount, status, created, data) VALUES (7, 0, 'void', '2025-02-24', NULL), (9, -1.50, 'new', '2025-02-10', '{"items": [{"sku": "a", "qty": 2}]}'), (10, NULL, 'paid', '2025-02-18', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 12345678.90, 'new', '2025-02-01', '{"gift": true}'), (1, 9.99, 'paid', '2025-02-14', '{"items": [{"sku": "a", "qty": 2}]}'), (11, 9.99, 'new', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (12, 12345678.90, 'void', '2025-02-12', '{"gift": true}'), (8, 100, 'new', '2025-02-03', '{"items": []}'), (9, -1.50, 'void', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (7, 100, 'new', '2025-02-08', '{"gift": true}'), (12, 100, 'paid', '2025-02-24', '{"gift": true}'), (8, 9.99, 'paid', '2025-02-20', '{"gift": true}'), (6, 100, 'new', '2025-02-19', '{"items": []}'), (2, 0, 'void', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (3, NULL, 'new', '2025-02-26', '{"gift": true}'), (2, 9.99, 'void', '2025-02-20', NULL), (6, 0, 'void', '2025-02-13', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 57, 'note 3', 0, -0, '2025-11-02 05:30:00+00'), (2, 9, 185, 'note 2', -12.125, NULL, NULL), (3, 2, 13, 'n%5', 0, 'NaN', '2025-03-09 06:59:59+00'), (4, 9, 105, 'note 2', 0.5, 2.25, '2025-03-09 07:00:00+00'), (5, 1, -39, 'NOTE 3', 1000, -0, NULL), (6, 1, 1, 'NOTE 3', 3.333, 2.25, '2025-03-09 07:00:00+00'), (7, 1, 142, 'note_4', NULL, 2.25, '2025-03-09 06:59:59+00'), (8, 1, 184, 'n%5', 3.333, 0.5, '2025-11-02 05:30:00+00'), (9, 6, -27, 'NOTE 3', 3.333, -0, '2025-11-02 05:30:00+00'), (10, 1, 53, 'note 2', NULL, 2.25, '2025-03-09 06:59:59+00'), (11, 1, 184, 'NOTE 3', 0.5, 0.5, '2025-11-02 05:30:00+00'), (12, 1, 121, '', 0.5, 'Infinity', '2025-03-09 07:00:00+00'), (13, 1, NULL, 'é', -12.125, 0.5, NULL), (14, 1, 103, 'note 2', 3.333, 'Infinity', '2025-03-09 06:59:59+00'), (15, 1, -33, 'n%5', NULL, 0.5, '2024-02-29 12:00:00+00'), (16, 1, 77, 'note 3', 1000, -0, NULL), (17, 1, 48, 'note_4', NULL, -0, '2025-03-09 07:00:00+00'), (18, 1, 162, 'NOTE 3', 0.5, 0.5, NULL), (19, 1, 196, 'n%5', 3.333, 'Infinity', '2025-03-09 07:00:00+00'), (20, 4, -5, 'NOTE 3', 3.333, 'Infinity', '2025-03-09 06:59:59+00'), (21, 1, 157, 'NOTE 3', -12.125, 1e300, '2025-11-02 05:30:00+00'), (22, 5, 65, 'note_4', 0, 0.5, '2024-02-29 12:00:00+00'), (23, 5, 54, 'note 3', -12.125, NULL, '2024-02-29 12:00:00+00'), (24, 3, 77, 'note 3', 0, 2.25, '2025-11-02 05:30:00+00'), (25, 1, -1, 'n%5', 1000, -0, '2025-11-02 05:30:00+00'), (26, 3, 198, 'note 1', NULL, 'Infinity', '2025-03-09 07:00:00+00'), (27, 7, 111, 'note 3', -12.125, 2.25, '2025-03-09 07:00:00+00'), (28, 1, -31, 'é', 1000, 'Infinity', '2024-02-29 12:00:00+00'), (29, 1, 68, '', -12.125, 2.25, NULL), (30, 1, 6, 'NOTE 3', 0, -0, '2024-02-29 12:00:00+00'), (31, 1, 33, '', NULL, 0.5, '2025-03-09 06:59:59+00'), (32, 1, 146, 'é', NULL, 1e300, '2025-11-02 05:30:00+00'), (33, 1, 199, 'note 3', 3.333, -0, NULL), (34, 1, 128, 'é', 0.5, 'Infinity', '2025-03-09 06:59:59+00');

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

INSERT INTO r7t VALUES (1, 'Æ', 'Æ'), (2, 'Ä', 'Ä'), (3, 'Ä', 'Ä'), (4, 'ä', 'ä'), (5, 'ß', 'ß'), (6, 'e', 'e'), (7, 'ss', 'ss'), (8, '9', '9'), (9, 'e', 'e'), (10, 'ä b', 'ä b');

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

INSERT INTO r8a VALUES (1, 3, 12, 3.75, 'b', '2025-01-01'::timestamp + interval '36 hours'), (2, 2, -4, 3.75, 'ab', '2025-01-01'::timestamp + interval '36 hours'), (3, 4, 8, 1.50, 'z', '2025-01-01'::timestamp + interval '33 hours'), (4, 4, -4, -2.25, NULL, '2025-01-01'::timestamp + interval '0 hours'), (5, 2, -1, NULL, 'ab', '2025-01-01'::timestamp + interval '207 hours'), (6, 2, 10, NULL, 'ab', '2025-01-01'::timestamp + interval '46 hours'), (7, 2, 20, -2.25, NULL, '2025-01-01'::timestamp + interval '153 hours'), (8, 1, 6, -2.25, 'B', '2025-01-01'::timestamp + interval '68 hours'), (9, 4, NULL, 1.50, 'ab', '2025-01-01'::timestamp + interval '201 hours'), (10, 2, 6, 10, 'B', '2025-01-01'::timestamp + interval '1 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (3, 6, 7), (3, 5, 2), (4, 5, 3), (5, 1, 8), (5, 1, 8), (6, 1, 4), (6, 4, 3), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-2,0)', '{}', 'empty', NULL), (2, '[1,3)', '{[1,3), [5, 7)}', 'empty', '(1.25,1.5)'), (3, '[5,)', '{[5,9), [11, 13)}', '[2025-03-01,infinity)', '(,0]'), (4, NULL, '{(,10)}', NULL, NULL), (5, '[-3,-2)', NULL, '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (6, '[0,)', '{}', 'empty', '[0,0]'), (7, '(2,2]', NULL, '[2025-03-01,infinity)', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'the quick brown fox'), (2, ''), (3, 'running rats'), (4, 'dog-eat-dog world'), (5, 'foxes and dogs'), (6, 'quick quick slow'), (7, 'quick quick slow'), (8, 'the quick brown fox'), (9, 'running rats'), (10, 'über café naïve');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2444) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2967) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3782) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a8613 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a8613 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 711, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 711) i;

ALTER TABLE r9a8613 ALTER COLUMN u SET STATISTICS 100;

ANALYZE r9a8613 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a8613 WHERE s = 'common'$$);

SELECT * FROM r9est($$SELECT * FROM r9a8613 WHERE nul IS NULL$$);

DROP TABLE r9a8613;

CREATE TABLE r9w69181 (a int, b text);

INSERT INTO r9w69181 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q69181 AS SELECT a, b FROM r9w69181 WHERE a >= $1 ORDER BY a;

EXECUTE r9q69181(1);

EXECUTE r9q69181(2);

ALTER TABLE r9w69181 ALTER COLUMN a SET DEFAULT 5;

EXECUTE r9q69181(1);

DEALLOCATE r9q69181;

DROP TABLE r9w69181;

CREATE FUNCTION r8f61179(a int, b int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text := ''; i int; j int;
BEGIN
  <<outer>> FOR i IN REVERSE a..b BY 3 LOOP
    CONTINUE WHEN i % 3 = 0;
    j := 0;
    <<inner>> LOOP
      j := j + 1;
      EXIT outer WHEN length(r) > 40;
      EXIT inner WHEN j >= i;
      CONTINUE outer WHEN j = 4;
      r := r || i || ':' || j || ' ';
    END LOOP inner;
    WHILE j > 0 AND i < 0 LOOP j := j - 1; r := r || 'w'; END LOOP;
  END LOOP outer;
  RETURN r;
END $$;

SELECT r8f61179(9, 1);

SELECT r8f61179(2, -2);

SELECT r8f61179(NULL, 1);

DROP FUNCTION r8f61179;

\s2 BEGIN;

\s2 UPDATE r7k SET v = coalesce(v, 0) + 10, grp = grp + 1 WHERE id = 2;

\s3 BEGIN;

\s3& UPDATE r7k SET v = 0 WHERE grp = 1 AND id <= 2 RETURNING id;

\s2 ROLLBACK;

\s3.;

\s3 COMMIT;

\s2 ROLLBACK;

\s3 ROLLBACK;

CREATE TABLE r9w45579 (a int, b text);

INSERT INTO r9w45579 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q45579 AS SELECT a, b FROM r9w45579 WHERE a >= $1 ORDER BY a;

EXECUTE r9q45579(1);

EXECUTE r9q45579(2);

ALTER TABLE r9w45579 ADD COLUMN c int;

EXECUTE r9q45579(1);

DEALLOCATE r9q45579;

DROP TABLE r9w45579;

BEGIN;

DECLARE r6k SCROLL WITH HOLD CURSOR FOR SELECT id, uid FROM r6o WHERE uid = 2 ORDER BY created, id;

SELECT name, is_holdable, is_scrollable FROM pg_cursors ORDER BY name;

SAVEPOINT r6f;

FETCH BACKWARD 2 FROM r6k;

ROLLBACK TO SAVEPOINT r6f;

SAVEPOINT r6f;

FETCH 2 FROM r6k;

ROLLBACK TO SAVEPOINT r6f;

COMMIT;

FETCH ALL FROM r6k;

CLOSE ALL;

EXECUTE r5qa(2);

SELECT name, parameter_types::text, result_types::text, from_sql FROM pg_prepared_statements ORDER BY name;

CREATE TABLE r9i65571 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i65571 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 2289) i;

CREATE INDEX r9i65571_x0 ON r9i65571 USING btree (a);

CREATE INDEX r9i65571_x1 ON r9i65571 USING btree (t text_pattern_ops);

CREATE INDEX ON r9i65571 USING btree (abs(a - 40));

ALTER TABLE r9i65571 ADD CONSTRAINT r9i65571_ex EXCLUDE USING gist (p WITH ~=);

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i65571 WHERE t LIKE 'ab%';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i65571;

\x3 name=r9x42281 types=25 describe p=b616233
SELECT id, t FROM r9s WHERE t > $1 ORDER BY t, id LIMIT 4;

\x3 name=r9x42281 p=t''
-;

\x3 name=r9x42281 rf=1 p=t'b'
-;

\x3 name=r9x42281 p=b61
-;

\x3 name=r9x42281 p=t'a'
-;

\x3 name=r9x42281 p=b61
-;

\x3 name=r9x42281 p=t'b'
-;

\x3 name=r9x42281 rows=2 p=b42
-;

\x3 name=r9x42281 p=t'z'
-;

\x3 name=r9x42281 p=t'é1'
-;

\s3 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x42281';

\s3 DEALLOCATE r9x42281;

\s3 RESET plan_cache_mode;

CREATE TABLE r8t36036 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t36036 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf36036() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t36036', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf36036_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t36036', r);
  RETURN NULL;
END $$;

CREATE TRIGGER j_bad AFTER INSERT ON r8t36036 REFERENCING OLD TABLE AS ot FOR EACH ROW EXECUTE FUNCTION r8tf36036_tt();

CREATE TRIGGER c_bus BEFORE UPDATE OR DELETE ON r8t36036 FOR EACH STATEMENT EXECUTE FUNCTION r8tf36036('stmt');

CREATE CONSTRAINT TRIGGER h_def AFTER INSERT OR UPDATE ON r8t36036 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION r8tf36036('deferred');

INSERT INTO r8t36036 VALUES (10, 4, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't36036';

INSERT INTO r8t36036 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t36036.v RETURNING *;

DELETE FROM r8log WHERE tg = 't36036';

DELETE FROM r8t36036 WHERE id = 3 RETURNING *;

DELETE FROM r8log WHERE tg = 't36036';

INSERT INTO r8t36036 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't36036';

BEGIN;

UPDATE r8t36036 SET s = 'deferred' WHERE id = 2;

SET CONSTRAINTS ALL IMMEDIATE;

COMMIT;

DELETE FROM r8log WHERE tg = 't36036';

DROP TABLE r8t36036;

DROP FUNCTION r8tf36036, r8tf36036_tt;

INSERT INTO r6u (id, email, name, n) SELECT id + 3, 's' || email, name, n FROM r6u WHERE id % 3 = 0 ON CONFLICT  DO NOTHING RETURNING *;

CREATE TABLE r9i16783 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i16783 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 212) i;

CREATE INDEX CONCURRENTLY ON r9i16783 USING hash (a);

-- statement
ALTER TABLE r9i16783 ADD CONSTRAINT r9i16783_ex EXCLUDE USING gist (r WITH &&);
