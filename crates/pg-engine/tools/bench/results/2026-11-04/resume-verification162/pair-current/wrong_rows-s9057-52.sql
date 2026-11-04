-- class: wrong_rows
-- ordered: false
-- note: 13 vs 10 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: r9i82575.a DESC, r9i82575.id
-- engine:         ->  Sort
-- engine:               Sort Key: r9i82575_1.a DESC, r9i82575_1.id
-- engine:               ->  Seq Scan on r9i82575_1
-- engine:                     Disabled: true
-- engine:                     Filter: (a < 5)
-- engine:         ->  Sort
-- engine:               Sort Key: r9i82575_2.a DESC, r9i82575_2.id
-- engine:               ->  Seq Scan on r9i82575_2
-- engine:                     Disabled: true
-- engine:                     Filter: (a < 5)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: r9i82575.a DESC, r9i82575.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i82575_1
-- reference:                     Disabled: true
-- reference:                     Filter: (a < 5)
-- reference:               ->  Seq Scan on r9i82575_2
-- reference:                     Disabled: true
-- reference:                     Filter: (a < 5)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4[], b d3int, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b) VALUES (1, NULL, 1), (31, NULL, NULL), (38, '{}', 0), (35, NULL, NULL), (18, NULL, NULL), (32, NULL, 0), (33, NULL, 6), (6, '{2}', 0);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 THEN RETURN NULL; END IF; NEW.b := coalesce(NEW.b, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamptz, b timestamp, c text COLLATE "ci3");

INSERT INTO t1 (id, a, b, c) VALUES (9, '2024-01-02 06:00:00+00', NULL, NULL), (39, '2024-01-09 00:00:00+00', '2024-01-03 00:00:00', 'a,b'), (38, NULL, NULL, 'b'), (10, NULL, '2024-01-03 00:00:00', 'B'), (36, NULL, NULL, 'ab'), (25, '2024-01-05 23:00:00+00', NULL, 'a,b'), (22, NULL, '2024-01-03 12:30:00', 'ba'), (3, '2024-01-01 23:00:00+00', NULL, 'A'), (55, NULL, '2024-01-03 06:00:00', NULL), (23, NULL, '2024-01-06 23:00:00', NULL), (29, NULL, '2024-01-08 06:30:00', NULL), (28, NULL, NULL, 'B'), (24, NULL, NULL, NULL), (13, '2024-01-07 12:00:00+00', NULL, 'ba'), (19, NULL, '2024-01-09 12:30:00', NULL), (37, NULL, '2024-01-01 06:30:00', NULL), (35, '2024-01-05 12:00:00+00', '2024-01-03 06:00:00', 'Ab'), (18, '2024-01-02 12:00:00+00', NULL, 'abc'), (17, '2024-01-03 06:00:00+00', '2024-01-08 23:00:00', 'a_b'), (32, '2024-01-09 12:00:00+00', '2024-01-06 12:00:00', NULL);

CREATE INDEX t1_i609 ON t1 (c);

CREATE INDEX t1_i939 ON t1 (a) WHERE a IS NOT NULL;

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql  STRICT AS $$ SELECT CASE WHEN false THEN position(quote_literal(q) in concat(q, p)) ELSE p END $$;

CREATE VIEW v0 AS SELECT (('1999-04-05 05:30:00'::timestamp + CASE WHEN NULL::bool THEN '-2 days'::interval ELSE '0'::interval END) - t1.b) AS c0, lpad(COALESCE(t2.c, array_to_string('{b,NULL}'::text[], ',', '?')), 3, '*') AS c1, mod((extract(epoch from t2.b) + (1::numeric + 100::numeric)), 3) AS c2 FROM (t1 t1 JOIN t1 t2 ON jsonb_strip_nulls('{}'::jsonb) @> jsonb_build_object('k', t2.id, 'v', 'a%')) WHERE false;

CREATE VIEW v1 AS SELECT (CASE WHEN true THEN CASE WHEN false THEN t1.a ELSE t1.a END ELSE (t1.b AT TIME ZONE 'UTC') END + s2.c1) AS c0, ((- (-4)) % 2) AS c1, '{"a": 2.50}'::jsonb AS c2 FROM (t1 t1 JOIN (SELECT '[]'::json AS c0, (('2024-10-25'::date)::timestamp - LEAST(('2000-12-26'::date)::timestamp, ('2024-06-24'::date)::timestamp)) AS c1 FROM (t0 t3 FULL JOIN t0 t4 ON t3.b = t4.id AND NULL::bool) WHERE (NOT (false)) IS NOT FALSE) s2 ON ('{}'::jsonb::jsonb ->> 'a')::int < f0(4, t1.c));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{}', '{}', '2024-02-29 23:00:00+00', NULL, '2025-01-01', NULL, '1.23456789012345678901234567890', '2', NULL), (2, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', NULL, '2000-01-01 00:00:00', '2024-02-29', '1 mon -30 days', '123.45', '-1.25', -3), (3, 'null', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', 'infinity', 'infinity', '23:59:59.999999', '1e30', '0.125', 5), (4, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', 'infinity', NULL, 'infinity', NULL, '2', 1);

INSERT INTO tp4 VALUES (5, NULL, 'ab', '-Infinity'), (6, NULL, 'd', 'NaN'), (7, NULL, 'ab', '0.5');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "pg_c_utf8", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 19, 'İ', '0.0000000000000000000000000001', '-178000000 years', '(,)', NULL, NULL, '{}'), (2, NULL, '2', '-1e40', '1 day 00:00:00.000001', 'empty', '{empty}', 'Fat cats! FAT rats? fat-free', NULL), (3, 8, NULL, 'Infinity', '0', '[1,5)', '{[1,2],[2,3)}', 'the and of', ARRAY[ROW(-2, 'E')::r5ct]), (4, -3, 'i', '123456789012345678901234567890.123456789', '30 days', '(,2]', '{[1,3),[5,7)}', 'Fat cats! FAT rats? fat-free', NULL), (5, 16, 'ab', 'NaN', '1 mon', NULL, '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(NULL, NULL)::r5ct, ROW(3, 'I')::r5ct, ROW(1, NULL)::r5ct]), (6, 17, 'I', '-Infinity', '30 days', '(,)', '{[0,100)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(NULL, 'A')::r5ct, ROW(NULL, 'B')::r5ct]), (7, -1, 'AB', '7', '-2147483648 days', 'empty', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(1, 'ß')::r5ct, ROW(5, 'İ')::r5ct]), (8, NULL, NULL, '-99999999999999999999999999999999999.999999', '178000000 years', '[1,5)', '{(,0],[10,)}', 'a fat cat sat on a mat and ate a fat rat', '{}');

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_b ON r5a (b COLLATE "sv-x-icu");

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 12 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (-2, 'a', '2025-01-01'), (18, 'b', NULL), (0, 'c', 'infinity'), (-4, 'd', NULL), (17, 'a', 'infinity'), (8, 'b', NULL), (7, 'c', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (26, 'x0', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob1', 1, '{a,b}', '[1, {"a": null}]'), (2, 'u2@x.org', 'zoë2', 2, NULL, '[1, {"a": null}]'), (3, 'u3@x.org', '3', 0, '{}', '{"seats": 1.50}'), (4, 'u4@x.org', 'bob 4', 4, NULL, '{"seats": 1.50}'), (5, 'u5@x.org', '5', 0, NULL, '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', 'Cid6', 6, '{a}', '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'Cid7', 7, NULL, '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (2, NULL, 'new', '2025-02-10', NULL), (7, NULL, 'new', '2025-02-17', '{"gift": true}'), (5, 9.99, 'paid', '2025-02-11', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 9.99, 'new', '2025-02-24', '{"items": []}'), (6, -1.50, 'void', '2025-02-10', '{"items": [{"sku": "a", "qty": 2}]}');

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

INSERT INTO r7k VALUES (1, 8, -37, 'note 2', NULL, 1e300, NULL), (2, 3, 37, 'é', 0.5, 'Infinity', '2025-03-09 07:00:00+00'), (3, 1, 197, 'n%5', 0, 2.25, NULL), (4, 1, 63, 'note 3', 1000, 0.5, '2025-11-02 05:30:00+00'), (5, 1, NULL, 'note_4', 3.333, 'Infinity', NULL), (6, 4, 129, 'note_4', 3.333, NULL, '2025-03-09 06:59:59+00'), (7, 1, 94, 'n%5', NULL, -0, NULL), (8, 1, 140, 'note_4', 3.333, 1e300, '2025-03-09 07:00:00+00'), (9, 6, 74, '', 1000, NULL, '2025-03-09 07:00:00+00'), (10, 9, 2, 'NOTE 3', NULL, 0.5, '2025-11-02 05:30:00+00'), (11, 1, -8, 'n%5', NULL, 'Infinity', NULL), (12, 4, -21, 'é', 3.333, 0.5, '2025-11-02 05:30:00+00'), (13, 1, 135, '', NULL, 0.5, '2025-11-02 05:30:00+00'), (14, 1, -31, '', 0, -0, '2024-02-29 12:00:00+00'), (15, 6, 87, 'note 2', 0.5, 'NaN', '2025-03-09 06:59:59+00'), (16, 2, 183, 'Note 3', 0, 'Infinity', '2025-11-02 05:30:00+00'), (17, 1, 42, 'é', 0.5, 'Infinity', '2025-03-09 06:59:59+00'), (18, 1, -42, 'note 2', NULL, -0, '2025-03-09 06:59:59+00'), (19, 9, 137, '', NULL, 0.5, '2025-11-02 05:30:00+00'), (20, 1, 192, 'Note 3', 3.333, 'Infinity', NULL), (21, 5, NULL, 'NOTE 3', 3.333, 'NaN', '2025-03-09 06:59:59+00'), (22, 1, 107, 'é', NULL, NULL, NULL), (23, 1, 132, 'note_4', 0.5, 0.5, NULL), (24, 1, -46, 'note 2', 0, 'NaN', '2025-03-09 07:00:00+00'), (25, 1, 12, 'note_4', -12.125, -0, NULL), (26, 1, 196, 'note_4', NULL, 0.5, '2025-03-09 07:00:00+00'), (27, 2, 56, 'note 3', 0, NULL, '2025-03-09 07:00:00+00'), (28, 9, -9, '', 0, NULL, '2025-03-09 07:00:00+00'), (29, 1, -20, '', 0, 'Infinity', '2025-03-09 06:59:59+00'), (30, 1, 113, 'n%5', NULL, 1e300, '2025-03-09 07:00:00+00'), (31, 1, 184, '', 0, NULL, '2024-02-29 12:00:00+00'), (32, 1, 98, 'NOTE 3', NULL, 2.25, NULL), (33, 1, 6, 'é', 0, 0.5, '2025-03-09 07:00:00+00');

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

INSERT INTO r7t VALUES (1, '9', '9'), (2, '9', '9'), (3, 'é', 'é'), (4, 'e', 'e'), (5, 'Æ', 'Æ'), (6, 'e', 'e'), (7, 'A', 'A'), (8, 'ss', 'ss'), (9, 'ae', 'ae');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, 4, NULL, 'a', '2025-01-01'::timestamp + interval '123 hours'), (2, 1, 9, 3.75, 'b', '2025-01-01'::timestamp + interval '63 hours'), (3, 4, NULL, -2.25, 'B', '2025-01-01'::timestamp + interval '29 hours'), (4, 3, 17, 0, 'ab', '2025-01-01'::timestamp + interval '168 hours'), (5, 1, 11, NULL, 'z', '2025-01-01'::timestamp + interval '68 hours'), (6, 3, 6, 3.75, NULL, '2025-01-01'::timestamp + interval '23 hours'), (7, 1, -5, 1.50, 'z', '2025-01-01'::timestamp + interval '16 hours'), (8, 1, 2, 10, NULL, '2025-01-01'::timestamp + interval '30 hours'), (9, 3, 20, 0, 'ab', '2025-01-01'::timestamp + interval '67 hours'), (10, 1, 10, 1.50, '', '2025-01-01'::timestamp + interval '58 hours'), (11, 3, NULL, 3.75, 'b', '2025-01-01'::timestamp + interval '9 hours'), (12, 3, 9, 1.50, 'ab', '2025-01-01'::timestamp + interval '71 hours'), (13, 4, 15, 1.50, NULL, '2025-01-01'::timestamp + interval '66 hours'), (14, 1, 15, 1.50, 'z', '2025-01-01'::timestamp + interval '20 hours'), (15, 3, 18, NULL, NULL, '2025-01-01'::timestamp + interval '40 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 5, 9), (3, 1, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, NULL, NULL, '[1.5,2.5)'), (2, 'empty', '{[-3,-1), [1, 3)}', NULL, NULL), (3, '[5,)', '{}', NULL, '(,0]'), (4, '[3,)', '{}', 'empty', '[1.5,2.5)'), (5, '(3,3]', '{(,3)}', '[2025-03-01,infinity)', '[0,0]'), (6, '[5,7)', '{[5,7), [9, 11)}', '[2025-01-01,2025-02-01)', '[0,0]'), (7, '[8,)', '{}', '[2025-03-01,infinity)', '(1.25,1.5)'), (8, '(10,11]', '{(,10)}', NULL, '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, ''), (2, 'a rat ran'), (3, 'foxes and dogs'), (4, 'jumps over the lazy dog'), (5, 'über café naïve'), (6, 'quick quick slow'), (7, 'a rat ran'), (8, 'foxes and dogs'), (9, 'dog-eat-dog world'), (10, 'dog-eat-dog world');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '[1, "two", 3.5, null, true, {"k": [10, 20]}]');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2535) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1980) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3707) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9c50258 (id int PRIMARY KEY, a int, b text, s serial) WITH (autovacuum_enabled = off);

CREATE UNIQUE INDEX r9c50258_b ON r9c50258 (b);

BEGIN;

COPY r9c50258 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
5000	44	b7
\.

\s2 SELECT count(*), max(id) FROM r9c50258;

ROLLBACK;

\s2 SELECT count(*), max(id), sum(a) FROM r9c50258;

DROP TABLE r9c50258;

CREATE TABLE r9a47430 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a47430 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4184, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4184) i;

UPDATE r9a47430 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a47430;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a47430 WHERE u < 100$$);

SELECT * FROM r9est($$SELECT * FROM r9a47430 WHERE g = 39$$);

SELECT * FROM r9est($$SELECT g, count(*) FROM r9a47430 GROUP BY g$$);

DROP TABLE r9a47430;

SELECT lo_from_bytea(933607, '\x')::int8 - 933607;

SELECT lo_get(933607), lo_get(933607, 1, 4);

BEGIN;

SELECT lo_open(933607, x'60000'::int);

SELECT loread(0, 3);

SELECT lo_lseek(0, 6, 3);

SELECT lowrite(0, '\x5a5a');

SELECT lo_truncate(0, 5);

SELECT lo_close(0);

ROLLBACK;

SELECT lo_get(933607);

SELECT oid::int8 - 933607, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = 933607;

SELECT lo_open(933608, x'40000'::int);

SELECT lo_unlink(933607);

SELECT lo_get(933607);

CREATE TABLE r9i82575 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i82575_1 PARTITION OF r9i82575 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i82575_2 PARTITION OF r9i82575 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i82575 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 250) i;

BEGIN;

CREATE INDEX r9i82575_x0 ON r9i82575 USING brin (a) WITH (pages_per_range = 2);

CREATE UNIQUE INDEX r9i82575_x1 ON r9i82575 USING btree (a);

CREATE INDEX r9i82575_x2 ON r9i82575 USING hash (a);

ALTER TABLE r9i82575 ADD CONSTRAINT r9i82575_ex EXCLUDE USING gist (p WITH ~=);

ROLLBACK;

SET enable_seqscan = off;

SET enable_indexonlyscan = off;

-- statement
EXPLAIN (COSTS OFF) SELECT id, a FROM r9i82575 WHERE a < 5 ORDER BY a DESC NULLS FIRST, id LIMIT 10;
