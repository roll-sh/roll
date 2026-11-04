-- class: wrong_error
-- ordered: false
-- note: 08P01 vs 54000
-- engine: PARAMS : 1007
-- engine: ERROR 08P01: insufficient data left in message
-- reference: PARAMS : 1007
-- reference: ERROR 54000: number of array dimensions (2066889017) exceeds the maximum allowed (6)

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a numeric(10,2), b date, c float8, d d3int, e text, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (6, NULL, NULL, -0.75, NULL, NULL), (30, NULL, NULL, NULL, 6, NULL), (1, NULL, '2024-03-18', NULL, 8, NULL);

CREATE INDEX t0_i581 ON t0 (c);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 4 THEN RETURN NULL; END IF; NEW.d := coalesce(NEW.d, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a d3int, b int4, c float8, d d3int, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t1 (id, a, b, c, d) VALUES (29, 3, 2, 0, -1), (5, NULL, 2, -0.75, 7), (12, 6, 2, -2, 5), (21, 3, -1, NULL, 4), (2, 1, 2, 2, 9), (28, 3, -1, 0, 4), (17, 1, 2, 3, 6), (25, 8, 3, 1.75, 9);

CREATE INDEX t1_i991 ON t1 (g);

CREATE INDEX t1_i898 ON t1 (b) WHERE b IS NOT NULL;

CREATE TABLE t2 (id int4 PRIMARY KEY, a d3int, b timestamp, c text COLLATE "C", d int8);

INSERT INTO t2 (id, a, b, c, d) VALUES (4, 2, '2024-01-05 06:30:00', 'ab', 13), (36, 6, '2024-01-06 06:00:00', 'B', 17), (13, 2, '2024-01-09 12:30:00', 'ß', 5), (55, 4, '2024-01-09 23:30:00', 'x ', 14), (33, NULL, NULL, 'a', 14), (28, NULL, NULL, '', 9), (5, 1, '2024-01-03 23:00:00', 'a,b', NULL), (23, 2, '2024-01-04 12:30:00', 'ab', 13), (26, NULL, '2024-01-06 23:00:00', NULL, 13), (6, 6, '2024-01-09 23:30:00', NULL, 16), (20, NULL, '2024-01-05 12:00:00', 'é', -2), (27, 2, '2024-01-07 00:30:00', ' x', -2);

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('Ä', 1)::ct3, ARRAY[ROW('a', 3)::ct3, ROW(NULL, 3)::ct3], 0), (2, ROW('B', 1)::ct3, ARRAY[ROW('aa', 2)::ct3, ROW('a_b', 2)::ct3], 4), (3, ROW('z', 3)::ct3, NULL, 3), (4, ROW(NULL, NULL)::ct3, '{}', 0), (5, ROW(NULL, 2)::ct3, '{}', 1);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_cd ON tc3 (c DESC NULLS LAST, n);

CREATE INDEX tc3_ca ON tc3 (ca);

ANALYZE;

CREATE VIEW v0 AS SELECT t2.c AS c0, date_trunc('month', date_trunc('week', CASE WHEN true THEN t2.b ELSE t2.b END)) AS c1 FROM t2 t2;

CREATE VIEW v1 AS SELECT GREATEST(('-2 days'::interval * position('' in 'Ä')), '2 hours'::interval) AS c0, 6 AS c1, JSON_ARRAY((100::numeric)::numeric(6,1), (SELECT every(false) FROM t2 t4 WHERE NULL::bool) RETURNING jsonb) >= '{}'::jsonb AS c2 FROM (SELECT '2000-03-17 17:00:00-05'::timestamptz AS c0 FROM v0 t2 , v0 t3 WHERE (NULLIF(true, false) AND (('2024-12-14'::date - '2025-06-25'::date), md5(t3.c0)) <= (gcd(7, 6), to_char(-0.25::numeric, '0000'))) GROUP BY t2.c1) s1 WHERE true;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '1970-01-01 00:00:00.5', NULL, '-1 mon 2 days', NULL, '-0', 3), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '1999-12-31 23:59:59.999999+00', NULL, '1999-12-31', '-1 year -1 day', '0.000001', NULL, NULL), (3, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2025-10-26 01:30:00+00', '2025-03-09 02:30:00', '-infinity', NULL, NULL, 'NaN', 11), (4, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2025-04-06 02:30:00+11', '2025-12-31 23:59:59.999999', 'infinity', '1 year 2 mons', '123.45', '0.5', -3), (5, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2038-01-19 03:14:08+00', 'infinity', NULL, '-12:00', 'NaN', NULL, 9), (6, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', NULL, '2000-03-01', '0', NULL, '-1.25', 1), (7, 'null', 'null', '2038-01-19 03:14:08+00', NULL, NULL, '-12:00', NULL, '-0', 7);

INSERT INTO tp4 VALUES (-1, '-infinity', 'b', '0.000001'), (2, NULL, 'c', '1.23456789012345678901234567890'), (14, '-infinity', 'b', 'NaN'), (4, NULL, 'b', '-7'), (16, NULL, NULL, '0'), (-3, 'infinity', NULL, '99999999999999999999'), (-3, '4713-01-01 BC', 'ab', '0'), (17, 'infinity', 'b', '1.23456789012345678901234567890');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5num", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 14, 'ä', 'NaN', '2147483647 mons', '[3,)', '{(,0],[10,)}', 'running runners ran; the runner runs', ARRAY[ROW(5, 'Ä')::r5ct]), (2, 17, ' a', '-0.5', '-1 mon 30 days', '[0,10)', '{[1,2],[2,3)}', 'the and of', ARRAY[ROW(NULL, 'a-')::r5ct, ROW(2, 'ä')::r5ct]), (3, 6, 'o', NULL, '30 days', '[1,5)', '{(,0],[10,)}', 'Supernovae stars are the brightest phenomena in galaxies', '{}'), (4, 12, 'B', 'NaN', '178000000 years', '[5,6)', '{empty}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL), (5, 4, 'ä', '1e100', '1 day 00:00:00.000001', '(,)', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(3, 'e')::r5ct, ROW(3, 'B')::r5ct]), (6, NULL, 'o', '-99999999999999999999999999999999999.999999', '-1 mon 30 days', '[-3,0]', '{[1,3),[5,7)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(5, 'I')::r5ct, ROW(1, '-a')::r5ct, ROW(5, 'o')::r5ct]);

CREATE INDEX r5a_b ON r5a (b COLLATE "und-x-icu");

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 30 WITH LOCAL CHECK OPTION;

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

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (-4, 'a', 'infinity'), (7, 'b', '2024-02-29'), (2, 'c', '2024-02-29'), (-4, 'd', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (31, 'x0', NULL), (27, 'x1', NULL), (26, 'x2', NULL), (20, 'x3', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Cid1', 1, '{a}', '[1, {"a": null}]'), (2, 'u2@x.org', 'Éva2', 0, '{a}', NULL), (3, 'u3@x.org', 'Éva3', 0, '{a,b}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', 'ann4', 4, '{b,NULL}', '{"seats": 1.50}'), (5, 'u5@x.org', 'ann5', 0, '{a}', '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', 'ann6', 0, '{b,NULL}', '{"seats": 1.50}'), (7, 'u7@x.org', 'Cid7', 0, '{a}', '{"seats": 1.50}'), (8, 'u8@x.org', '8', 8, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (5, 12345678.90, 'void', '2025-02-01', '{"items": []}'), (5, 100, 'paid', '2025-02-13', '{"items": [{"sku": "a", "qty": 2}]}'), (7, -1.50, 'new', '2025-02-15', '{"gift": true}'), (8, -1.50, 'void', '2025-02-23', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 100, 'void', '2025-02-02', NULL), (7, 12345678.90, 'paid', '2025-02-25', '{"gift": true}'), (2, -1.50, 'new', '2025-02-02', NULL), (1, -1.50, 'void', '2025-02-27', '{"gift": true}'), (8, 9.99, 'new', '2025-02-20', '{"gift": true}'), (1, -1.50, 'void', '2025-02-12', '{"gift": true}'), (7, 12345678.90, 'paid', '2025-02-11', '{"gift": true}'), (8, -1.50, 'void', '2025-02-11', '{"gift": true}'), (3, 12345678.90, 'new', '2025-02-16', NULL);

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

INSERT INTO r7k VALUES (1, 1, 95, 'NOTE 3', NULL, 'NaN', '2025-03-09 07:00:00+00'), (2, 1, 95, 'note 3', NULL, 1e300, NULL), (3, 1, 194, 'Note 3', 0, 'NaN', '2025-03-09 07:00:00+00'), (4, 8, 17, 'n%5', 3.333, 'NaN', '2025-03-09 06:59:59+00'), (5, 5, -2, 'note 2', 3.333, 'Infinity', '2025-03-09 06:59:59+00'), (6, 1, 64, 'n%5', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (7, 1, 191, 'note 1', 0.5, 'Infinity', '2025-11-02 05:30:00+00'), (8, 1, 32, 'é', 1000, NULL, NULL), (9, 1, NULL, 'note 2', 3.333, -0, '2025-03-09 06:59:59+00'), (10, 1, 162, 'note 3', 0, -0, '2025-03-09 06:59:59+00'), (11, 1, 25, 'note 1', NULL, 0.5, '2025-03-09 06:59:59+00'), (12, 7, -7, '', 0, 0.5, '2025-11-02 05:30:00+00'), (13, 3, -43, 'n%5', 0.5, -0, '2024-02-29 12:00:00+00'), (14, 1, 8, 'note 3', 1000, -0, '2024-02-29 12:00:00+00'), (15, 2, 36, 'note 2', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (16, 9, -14, 'note 2', NULL, 'NaN', NULL), (17, 7, 59, 'n%5', NULL, 0.5, '2025-03-09 07:00:00+00'), (18, 1, 48, '', NULL, 2.25, '2025-03-09 06:59:59+00'), (19, 2, 46, 'note 3', 3.333, 1e300, NULL), (20, 1, 183, 'n%5', 0, NULL, '2025-03-09 07:00:00+00'), (21, 4, 98, 'n%5', NULL, NULL, '2024-02-29 12:00:00+00'), (22, 1, 200, 'n%5', -12.125, NULL, NULL), (23, 4, 55, 'note 3', 0.5, NULL, '2025-03-09 07:00:00+00'), (24, 7, -34, 'note 1', 0, 'NaN', '2025-03-09 07:00:00+00'), (25, 6, 92, 'Note 3', 0.5, 'Infinity', '2025-11-02 05:30:00+00'), (26, 1, 11, 'note 2', NULL, 'NaN', NULL), (27, 1, 4, 'note 3', 3.333, 2.25, '2025-03-09 06:59:59+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5);

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

INSERT INTO r7t VALUES (1, 'E', 'E'), (2, 'e', 'e'), (3, 'ss', 'ss'), (4, 'Æ', 'Æ'), (5, 'é', 'é'), (6, 'ä', 'ä'), (7, 'b', 'b'), (8, '9', '9'), (9, 'ß', 'ß'), (10, 'a', 'a'), (11, 'a', 'a');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, 6, 0, 'a', '2025-01-01'::timestamp + interval '54 hours'), (2, 2, NULL, NULL, 'a', '2025-01-01'::timestamp + interval '65 hours'), (3, 1, 20, -2.25, 'z', '2025-01-01'::timestamp + interval '195 hours'), (4, 1, 0, NULL, 'z', '2025-01-01'::timestamp + interval '94 hours'), (5, 4, 14, NULL, 'B', '2025-01-01'::timestamp + interval '90 hours'), (6, 1, NULL, -2.25, 'B', '2025-01-01'::timestamp + interval '63 hours'), (7, 3, 6, 3.75, 'z', '2025-01-01'::timestamp + interval '13 hours'), (8, 2, -4, 1.50, '', '2025-01-01'::timestamp + interval '32 hours'), (9, 2, -2, 3.75, 'B', '2025-01-01'::timestamp + interval '135 hours'), (10, 3, -5, 1.50, '', '2025-01-01'::timestamp + interval '61 hours'), (11, 4, 4, 0, 'a', '2025-01-01'::timestamp + interval '56 hours'), (12, 4, NULL, 10, 'B', '2025-01-01'::timestamp + interval '18 hours'), (13, 4, 11, 0, NULL, '2025-01-01'::timestamp + interval '134 hours'), (14, 2, 15, -2.25, 'a', '2025-01-01'::timestamp + interval '112 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (5, 4, 8), (6, 2, 4), (6, 5, 3), (7, 8, 4), (7, 2, 8), (8, 6, 5), (8, 7, 1), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[1,)', '{[1,7), [9, 11)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (2, 'empty', '{}', NULL, '[0,0]'), (3, '[6,7)', '{}', 'empty', '[0,0]'), (4, 'empty', '{[3,8), [10, 12)}', '(2024-12-31,2025-01-05]', NULL);

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'the quick brown fox'), (2, 'a rat ran'), (3, 'the quick brown fox'), (4, 'quick quick slow'), (5, 'jumps over the lazy dog');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3518) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2712) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2028) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

PREPARE r9p30599 (int) AS SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

EXECUTE r9p30599(3000000000);

EXECUTE r9p30599(7);

EXECUTE r9p30599(NULL);

EXECUTE r9p30599(0);

EXECUTE r9p30599(NULL);

EXPLAIN (COSTS OFF) EXECUTE r9p30599(-50);

EXECUTE r9p30599(-50);

EXECUTE r9p30599(NULL);

CREATE INDEX r9s_v30599 ON r9s (v);

DROP INDEX r9s_v30599;

EXECUTE r9p30599(-1);

EXECUTE r9p30599(7);

EXPLAIN (COSTS OFF) EXECUTE r9p30599(NULL);

EXECUTE r9p30599(NULL);

EXPLAIN (COSTS OFF) EXECUTE r9p30599(49);

EXECUTE r9p30599(49);

EXECUTE r9p30599(3000000000);

EXECUTE r9p30599(0);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p30599';

DEALLOCATE r9p30599;

RESET plan_cache_mode;

CREATE TABLE r9i51002 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i51002 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 286) i;

ANALYZE r9i51002;

BEGIN;

CREATE INDEX ON r9i51002 USING brin (id);

CREATE INDEX r9i51002_x1 ON r9i51002 USING spgist (p);

ROLLBACK;

REINDEX TABLE r9i51002;

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i51002 WHERE lower(t) = 'abc3';

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i51002 WHERE p <@ box '((0,0),(5,5))';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i51002;

CREATE FUNCTION r8f52387(m int, OUT a int, OUT b text) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, t FROM r8a WHERE id <= m ORDER BY id;
  RETURN QUERY EXECUTE 'SELECT $1 * 100, $2' USING m, 'x';
  a := -1; b := NULL; RETURN NEXT;
  IF m < 0 THEN RETURN QUERY SELECT 1, 2; END IF;
END $$;

SELECT * FROM r8f52387(2);

SELECT a, b FROM r8f52387(3) ORDER BY a, b;

SELECT * FROM r8f52387(-1);

DROP FUNCTION r8f52387;

SELECT jsonb_set('{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}', '{b,0}', '"n"', true), jsonb_insert('{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}', '{b,-1}', '9', true), '{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}'::jsonb - 'a', '{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}'::jsonb #- '{b,1}', jsonb_set_lax('{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}', '{a}', NULL, true, 'return_target');

CREATE TABLE r9a95488 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a95488 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1193, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1193) i;

ALTER TABLE r9a95488 ALTER COLUMN s SET STATISTICS 4;

SET default_statistics_target = 25;

ALTER TABLE r9a95488 ALTER COLUMN g SET (n_distinct = -1);

ANALYZE (SKIP_LOCKED) r9a95488;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT g, s FROM r9a95488 GROUP BY g, s$$);

SELECT * FROM r9est($$SELECT * FROM r9a95488 WHERE f BETWEEN 20 AND 30$$);

SELECT * FROM r9est($$SELECT * FROM r9a95488 WHERE g IN (1, 2, 3) AND b$$);

EXPLAIN SELECT * FROM r9a95488 WHERE g IN (1, 2, 3) AND b;

DROP TABLE r9a95488;

CREATE TABLE r8c63917 (id int NOT NULL, a text, b int DEFAULT 42, c date);

COPY r8c63917 (id, a, b, c) FROM STDIN (FORMAT csv, ON_ERROR ignore, REJECT_LIMIT 2, DEFAULT '\D');
1,,
2,,-7,2025-01-31
3,multi word,3,2025-02-29
4,x,,2025-13-01
5,x,zz,
\.

COPY r8c63917 TO STDOUT (FORMAT csv, FORCE_QUOTE (a), NULL 'nil', DELIMITER '|');

COPY r8c63917 FROM STDIN (DELIMITER ',,');

DROP TABLE r8c63917;

CREATE TABLE r9a3051 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a3051 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1375, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1375) i;

ALTER TABLE r9a3051 ALTER COLUMN s SET STATISTICS 13;

CREATE STATISTICS r9a3051_st (dependencies) ON g, u, b FROM r9a3051;

UPDATE r9a3051 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a3051 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a3051 WHERE s = 'v7'$$);

DROP TABLE r9a3051;

\s2 BEGIN;

\s2 UPDATE r7k SET v = coalesce(v, 0) + 10, grp = grp + 1 WHERE id = 1;

\s3 BEGIN;

\s3& SELECT id, v FROM r7k WHERE id = 1 FOR NO KEY UPDATE;

\s2 ROLLBACK;

\s3.;

\s3 COMMIT;

\s2 ROLLBACK;

\s3 ROLLBACK;

CREATE TABLE r9a23277 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a23277 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 5159, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 5159) i;

CREATE STATISTICS r9a23277_st ON g, s FROM r9a23277;

UPDATE r9a23277 SET g = 5, nul = NULL WHERE id % 50 = 0;

VACUUM ANALYZE r9a23277;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a23277 WHERE g = 39$$);

SELECT * FROM r9est($$SELECT * FROM r9a23277 WHERE s = 'v7'$$);

SELECT * FROM r9est($$SELECT * FROM r9a23277 WHERE NOT b$$);

DROP TABLE r9a23277;

SET plan_cache_mode = force_generic_plan;

PREPARE r9p66426 (int, int) AS SELECT count(*) FROM r9s WHERE g = $1 AND v > 10 / $2;

EXECUTE r9p66426(2, 2);

EXPLAIN (COSTS OFF) EXECUTE r9p66426(99, 1);

EXECUTE r9p66426(99, 1);

EXECUTE r9p66426(3, 2);

EXECUTE r9p66426(5, 1);

EXECUTE r9p66426(1, -1);

EXECUTE r9p66426(2, 2);

EXECUTE r9p66426(1, 7);

EXECUTE r9p66426(3, 2);

EXECUTE r9p66426(1, 1);

EXECUTE r9p66426(8, 0);

EXECUTE r9p66426(1, -1);

EXECUTE r9p66426(2, 7);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p66426';

DEALLOCATE r9p66426;

RESET plan_cache_mode;

-- statement
\x2 name=r9x22011 describe rf=1,0 p=b7b323939392c317d
SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;
