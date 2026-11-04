-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: r9est:25
-- engine: HashAggregate  (rows=1600)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a7268  (rows=2426)
-- engine: -- SELECT 3
-- reference: r9est:25
-- reference: HashAggregate  (rows=60)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a7268  (rows=2426)
-- reference: -- SELECT 3

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b bool, c int4[], d int8, e float8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (35, -1, true, '{3,1,2}', 9, NULL), (10, 1, false, '{5}', 13, 1.25), (19, 4, false, '{5}', 4, -1.5);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 THEN RETURN NULL; END IF; NEW.a := coalesce(NEW.a, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (a text, b int4) PARTITION BY LIST (a);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES IN ('a', 'b', 'A');

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

INSERT INTO t1 (a, b) VALUES (NULL, NULL), (NULL, 7), ('a%', NULL), (NULL, NULL), (NULL, NULL), ('a', NULL), (NULL, NULL), ('Ab', NULL), (NULL, -1), ('a%', 7), (NULL, NULL), (NULL, NULL);

CREATE INDEX t1_i466 ON t1 (b, a);

CREATE INDEX t1_i651 ON t1 (a);

ANALYZE;

CREATE VIEW v0 AS SELECT (count(*))::int AS c0, GROUPING(t1.a, (t1.b)) FROM t1 t1 WHERE NOT EXISTS (SELECT 3 FROM t0 t2 ORDER BY 1 LIMIT 4) GROUP BY ROLLUP (t1.a, (t1.b));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2024-02-29 23:00:00+00', '0001-01-01 00:00:00 BC', '-infinity', NULL, NULL, '1024', 4), (2, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '1970-01-01 00:00:00.5', '2025-03-09', NULL, '1e30', NULL, 3), (3, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '2024-02-29 00:00:00', NULL, '0', '0', '-1.25', 8), (4, 'null', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', NULL, '-infinity', '-infinity', '0.000001', '0.5', 11);

INSERT INTO tp4 VALUES (-1, '2024-02-29', 'ab', '-Infinity'), (12, '2000-03-01', 'ab', NULL), (0, NULL, 'a', '1.23456789012345678901234567890'), (17, '4713-01-01 BC', 'c', 'NaN'), (19, '-infinity', 'b', '0'), (8, '2025-01-01', NULL, '0.000001'), (-4, '2025-01-01', 'd', '2.5'), (14, '4713-01-01 BC', 'a', '123.45'), (-2, NULL, 'b', '-2.5'), (25, '2000-03-01', 'd', '2.5'), (12, '4713-01-01 BC', 'a', 'NaN'), (4, '4713-01-01 BC', 'ab', '123.45');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 8, 'İ', '1e100', '2147483647 mons', '[3,)', '{}', '', ARRAY[ROW(3, 'b')::r5ct, ROW(-2, NULL)::r5ct, ROW(-1, 'å')::r5ct]), (2, 2, 'o', '-1e40', '720:00:00', '(,)', '{empty}', 'The quick brown fox jumps over the lazy dog', '{}'), (3, 0, 'ß', '2.5', '1 mon', '(1,5]', '{empty}', '', NULL), (4, 7, 'AB', '7', '-178000000 years', '[5,6)', '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(3, 'h')::r5ct]), (5, 6, ' a', NULL, '1 year -1 sec', '[-3,0]', '{[2,4)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(1, 'o')::r5ct, ROW(5, '10')::r5ct, ROW(1, 'B')::r5ct]), (6, NULL, 'h', '123456789012345678901234567890.123456789', 'infinity', '(,)', '{[0,100)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(NULL, 'b')::r5ct, ROW(4, 'ö')::r5ct, ROW(5, 'B')::r5ct]), (7, 11, 'ss', '0', '-infinity', '[5,6)', '{[2,4)}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (8, NULL, 'ch', '0', '1 year -1 sec', '[0,10)', '{[2,4)}', '', ARRAY[ROW(NULL, 'Ä')::r5ct, ROW(2, 'I')::r5ct]), (9, NULL, 'i', NULL, '-178000000 years', '[1,5)', '{(,0],[10,)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(1, NULL)::r5ct]);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 17 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (13, 'a', '2025-01-01'), (-1, 'b', NULL), (19, 'c', '2024-02-29'), (13, 'd', 'infinity'), (12, 'a', '2025-01-01'), (0, 'b', '2025-01-01'), (8, 'c', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (28, 'x0', NULL), (27, 'x1', NULL), (29, 'x2', NULL), (27, 'x3', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', '1', 1, '{}', NULL), (2, 'u2@x.org', 'bob2', 2, '{}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'bob3', 3, '{}', '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', 'zoë4', 4, '{}', '{"plan": "pro", "seats": 3}'), (5, 'u5@x.org', 'ann5', 5, '{a,b}', '{"plan": "pro", "seats": 3}'), (6, 'u6@x.org', 'ann6', 6, '{}', '[1, {"a": null}]'), (7, 'u7@x.org', 'dee7', 7, '{b,NULL}', NULL), (8, 'u8@x.org', 'ann8', 0, '{}', '{}'), (9, 'u9@x.org', 'ann9', 9, '{b,NULL}', '{}'), (10, 'u10@x.org', 'zoë10', 0, NULL, '{"plan": "free", "tags": [1, 2]}'), (11, 'u11@x.org', 'Cid11', 11, '{b,NULL}', '[1, {"a": null}]'), (12, 'u12@x.org', 'ann12', 0, NULL, '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (8, 100, 'new', '2025-02-10', '{"items": []}'), (12, 12345678.90, 'paid', '2025-02-13', '{"gift": true}'), (9, 100, 'paid', '2025-02-24', '{"items": []}'), (8, -1.50, 'paid', '2025-02-21', '{"items": [{"sku": "a", "qty": 2}]}'), (4, NULL, 'void', '2025-02-22', NULL), (4, 0, 'new', '2025-02-14', NULL), (10, 12345678.90, 'paid', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 9.99, 'void', '2025-02-27', '{"items": []}'), (10, NULL, 'void', '2025-02-22', '{"items": []}'), (1, 100, 'paid', '2025-02-03', '{"items": [{"sku": "a", "qty": 2}]}'), (10, 12345678.90, 'void', '2025-02-17', '{"gift": true}'), (12, 0, 'new', '2025-02-18', '{"items": []}'), (2, 12345678.90, 'paid', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (10, 100, 'void', '2025-02-19', '{"items": [{"sku": "a", "qty": 2}]}'), (9, 100, 'paid', '2025-02-03', '{"gift": true}'), (1, -1.50, 'paid', '2025-02-05', NULL), (7, 12345678.90, 'paid', '2025-02-21', '{"items": [{"sku": "a", "qty": 2}]}'), (12, 9.99, 'new', '2025-02-19', '{"items": [{"sku": "a", "qty": 2}]}'), (3, -1.50, 'new', '2025-02-07', NULL), (9, NULL, 'void', '2025-02-10', NULL), (10, 9.99, 'new', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (11, 0, 'new', '2025-02-04', '{"items": []}'), (8, 9.99, 'paid', '2025-02-06', '{"gift": true}'), (5, -1.50, 'void', '2025-02-27', NULL);

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

INSERT INTO r7k VALUES (1, 1, 114, 'n%5', 3.333, 'NaN', '2025-03-09 07:00:00+00'), (2, 6, 14, 'Note 3', NULL, 'NaN', '2025-03-09 06:59:59+00'), (3, 1, 189, 'é', 3.333, 'NaN', '2024-02-29 12:00:00+00'), (4, 8, 66, 'note 2', 3.333, -0, '2025-03-09 06:59:59+00'), (5, 7, 128, 'é', 0, 1e300, '2025-03-09 06:59:59+00'), (6, 9, 123, '', NULL, 2.25, '2025-03-09 07:00:00+00'), (7, 1, 190, 'Note 3', NULL, 2.25, '2025-03-09 07:00:00+00'), (8, 1, NULL, '', 1000, 1e300, NULL), (9, 1, -31, 'NOTE 3', NULL, 0.5, '2025-03-09 07:00:00+00'), (10, 1, 120, '', -12.125, 0.5, '2025-03-09 06:59:59+00'), (11, 6, 15, 'note 1', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (12, 1, 59, 'note 1', 0.5, -0, '2024-02-29 12:00:00+00'), (13, 9, NULL, 'n%5', 0.5, 2.25, NULL), (14, 1, NULL, 'é', 3.333, -0, '2025-03-09 06:59:59+00'), (15, 1, 56, 'é', 0.5, -0, '2024-02-29 12:00:00+00'), (16, 1, 27, '', 0.5, 'Infinity', NULL), (17, 1, 45, 'note 3', 0, 1e300, '2025-03-09 06:59:59+00'), (18, 1, 161, 'note 2', 3.333, 0.5, '2024-02-29 12:00:00+00'), (19, 5, 131, 'note 3', NULL, 'NaN', '2025-03-09 07:00:00+00'), (20, 9, -24, 'é', 0, 2.25, NULL);

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

INSERT INTO r7t VALUES (1, 'z', 'z'), (2, 'a', 'a'), (3, 'A', 'A'), (4, 'Ä', 'Ä'), (5, '', '');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 1, 10, NULL, 'b', '2025-01-01'::timestamp + interval '39 hours'), (2, 3, 15, -2.25, 'z', '2025-01-01'::timestamp + interval '46 hours'), (3, 4, NULL, 1.50, 'b', '2025-01-01'::timestamp + interval '3 hours'), (4, 1, 20, -2.25, 'a', '2025-01-01'::timestamp + interval '2 hours'), (5, 2, 20, 1.50, 'a', '2025-01-01'::timestamp + interval '24 hours'), (6, 3, 5, -2.25, NULL, '2025-01-01'::timestamp + interval '24 hours'), (7, 4, 7, 1.50, 'B', '2025-01-01'::timestamp + interval '20 hours'), (8, 1, 19, 1.50, 'b', '2025-01-01'::timestamp + interval '69 hours'), (9, 4, 14, 1.50, 'z', '2025-01-01'::timestamp + interval '66 hours'), (10, 1, 12, -2.25, 'b', '2025-01-01'::timestamp + interval '24 hours'), (11, 2, 5, -2.25, '', '2025-01-01'::timestamp + interval '21 hours'), (12, 4, 4, -2.25, NULL, '2025-01-01'::timestamp + interval '142 hours'), (13, 1, 0, 10, 'B', '2025-01-01'::timestamp + interval '28 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 7, 8), (2, 7, 7), (2, 4, 2), (3, 8, 4), (5, 8, 3), (6, 5, 1), (7, 8, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[5,8)', NULL, '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (2, '(2,5]', '{}', 'empty', '(1.25,1.5)'), (3, '(2,5]', NULL, '[2025-03-01,infinity)', '(,0]'), (4, '(3,8]', '{(,3)}', 'empty', '[0,0]'), (5, '[6,12)', NULL, 'empty', '[1.5,2.5)'), (6, 'empty', '{}', 'empty', '(1.25,1.5)'), (7, '[3,6)', '{(,3)}', 'empty', '(1.25,1.5)'), (8, NULL, '{[9,11), [13, 15)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (9, '(-1,2]', '{}', '[2025-03-01,infinity)', NULL);

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, 'foxes and dogs'), (3, 'dog-eat-dog world'), (4, 'über café naïve'), (5, 'jumps over the lazy dog'), (6, 'dog-eat-dog world'), (7, 'running rats'), (8, 'the quick brown fox'), (9, 'foxes and dogs');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '"scalar"'), (4, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3718) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2854) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3332) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9c40713 (id int PRIMARY KEY, a int, b text, s serial) WITH (autovacuum_enabled = off);

COPY r9c40713 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
7	44	b7
8	1	b8
9	8	b9
5	15	b10
11	22	b11
\.

DROP TABLE r9c40713;

CREATE TABLE r9a7268 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a7268 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2426, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2426) i;

ALTER TABLE r9a7268 ALTER COLUMN f SET STATISTICS -1;

ALTER TABLE r9a7268 ALTER COLUMN s SET (n_distinct = 0);

CREATE STATISTICS r9a7268_st ON g, (u % 10) FROM r9a7268;

ANALYZE r9a7268 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a7268 WHERE g = 3$$);

SELECT * FROM r9est($$SELECT * FROM r9a7268 WHERE nul IS NULL$$);

-- statement
SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a7268$$);
