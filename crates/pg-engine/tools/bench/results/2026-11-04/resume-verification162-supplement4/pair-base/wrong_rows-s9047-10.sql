-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: r9est:25
-- engine: HashAggregate  (rows=1600)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a44418  (rows=4402)
-- engine: -- SELECT 3
-- reference: r9est:25
-- reference: HashAggregate  (rows=60)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a44418  (rows=4402)
-- reference: -- SELECT 3

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a jsonb, b float8 DEFAULT -0.5, c text[], d timestamptz, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t0 (id, a, b, c, d) VALUES (36, '{"a": 1, "b": "x"}', 0.25, '{z}', '2024-01-06 23:00:00+00'), (12, '{"a": "1"}', -0.5, '{}', NULL), (29, '"s"', 1, '{}', '2024-01-05 00:00:00+00'), (7, NULL, 4, '{A,a}', '2024-01-06 00:00:00+00'), (22, '1', NULL, '{b,NULL}', '2024-01-06 23:00:00+00'), (17, '{"b": [1, "a"]}', -0.75, '{ab}', '2024-01-08 23:00:00+00'), (18, 'null', 0.25, '{z}', '2024-01-09 06:00:00+00'), (1, '"s"', -1, '{A,a}', '2024-01-03 12:00:00+00'), (37, '{"a": 3}', NULL, '{A,a}', '2024-01-07 12:00:00+00'), (27, NULL, NULL, '{z}', '2024-01-02 06:00:00+00'), (24, '1', -0.75, '{}', '2024-01-08 00:00:00+00'), (8, '{"a": {"b": 2}}', 1.25, '{A,a}', '2024-01-03 23:00:00+00'), (4, '{"a": 3}', 4, '{z}', '2024-01-08 12:00:00+00'), (34, '{"a": 3}', 1.75, '{ab}', '2024-01-03 06:00:00+00'), (52, 'null', -1, '{b,NULL}', '2024-01-06 12:00:00+00'), (16, '[1, 2, 3]', -2, '{}', '2024-01-04 12:00:00+00'), (13, '{"a": 3}', 2.5, NULL, '2024-01-09 12:00:00+00'), (53, '"s"', 1, '{}', '2024-01-03 12:00:00+00'), (31, 'null', -2, '{a,b}', '2024-01-01 23:00:00+00'), (38, '{"a": 1, "b": "x"}', 3.25, '{ab}', '2024-01-01 06:00:00+00'), (35, '{"a": 3}', NULL, '{}', '2024-01-08 06:00:00+00'), (39, '{"a": 3}', NULL, NULL, '2024-01-01 00:00:00+00'), (45, '{"a": {"b": 2}}', 0.75, '{}', '2024-01-08 00:00:00+00'), (30, '[]', 3.25, '{b,NULL}', '2024-01-09 23:00:00+00'), (2, '{"a": {"b": 2}}', NULL, '{z}', '2024-01-02 12:00:00+00'), (49, '1', -1.25, '{z}', '2024-01-05 23:00:00+00'), (28, 'null', 0.25, '{}', '2024-01-08 06:00:00+00'), (50, NULL, 0.5, '{b,NULL}', '2024-01-06 00:00:00+00'), (26, '[]', -0.5, NULL, '2024-01-06 23:00:00+00'), (57, '{"a": 3}', -0.25, '{ab}', NULL);

CREATE INDEX t0_i293 ON t0 USING hash (d);

CREATE INDEX t0_i536 ON t0 USING hash (d);

CREATE INDEX t0_i160 ON t0 (id) WHERE id IS NOT NULL;

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a numeric(10,2), b bool, c text COLLATE "ci3", g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

CREATE INDEX t1_i566 ON t1 (id, c);

CREATE INDEX t1_i867 ON t1 (a);

CREATE TABLE t2 (id int4 PRIMARY KEY, a numeric(10,2), b numeric(10,2) DEFAULT 0, c text COLLATE "POSIX", d float8);

INSERT INTO t2 (id, a, b, c, d) VALUES (25, NULL, NULL, 'b', NULL), (6, NULL, NULL, NULL, NULL), (24, NULL, NULL, NULL, -1.5), (36, NULL, 3.25, NULL, NULL), (22, 9.05, NULL, NULL, NULL), (16, NULL, 3.00, 'abc', NULL), (39, NULL, 0.00, NULL, NULL), (9, 5.00, NULL, 'Ab', NULL), (23, NULL, -2.00, NULL, NULL), (7, 3.50, 11.25, NULL, NULL), (11, NULL, NULL, NULL, NULL), (38, NULL, 7.50, 'a,b', NULL);

CREATE INDEX t2_i148 ON t2 (b DESC NULLS LAST);

CREATE INDEX t2_i653 ON t2 (c);

CREATE INDEX t2_i14 ON t2 ((id + 1));

CREATE TABLE t3 (id int4 PRIMARY KEY, a timestamp, b int2, c interval, d bool);

INSERT INTO t3 (id, a, b, c, d) VALUES (38, '2024-01-01 06:00:00', 3, '1 mon', false), (36, '2024-01-08 06:00:00', 3, '0', false), (29, '2024-01-03 06:30:00', NULL, '0', false), (20, '2024-01-01 12:00:00', 0, '2 hours', false), (8, '2024-01-08 06:00:00', 1, '-1 day', false);

CREATE INDEX t3_i4 ON t3 (b);

CREATE INDEX t3_i864 ON t3 (c);

CREATE TYPE ct3 AS (a text, b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('bar', 0)::ct3, NULL, 4), (2, ROW('a,b', 1)::ct3, '{}', NULL), (3, ROW(' x', 0)::ct3, NULL, NULL), (4, ROW(NULL, NULL)::ct3, ARRAY[ROW('foo', 1)::ct3, ROW(' x', NULL)::ct3], 1);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE STRICT AS $$ SELECT CASE WHEN false THEN (('1999-10-28'::date - '2000-01-22'::date) - p) ELSE ((-7.125::numeric / 2.5))::int4 END $$;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '"str"', '"str"', '1999-12-31 23:59:59.999999+00', '2025-10-26 02:30:00', '-infinity', '1 day', '2.5', 'NaN', 8), (2, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-11-02 01:30:00-05', '1970-01-01 00:00:00.5', '2025-03-09', '23:59:59.999999', '1.23456789012345678901234567890', 'NaN', 9), (3, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2038-01-19 03:14:08+00', '2024-02-29 00:00:00', '2025-01-01', '-1 mon 2 days', 'NaN', '-3', 8), (4, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-03-09 03:30:00-04', '1970-01-01 00:00:00.5', 'infinity', '1 mon -30 days', '99999999999999999999', '0.5', 3), (5, '{}', '{}', '2025-04-06 02:30:00+11', '2025-10-26 02:30:00', '2024-02-29', NULL, NULL, 'NaN', -2), (6, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2038-01-19 03:14:08+00', '2025-11-02 01:30:00', '4713-01-01 BC', '1 mon -30 days', '-7', '-3', 10);

INSERT INTO tp4 VALUES (-3, '1999-12-31', 'a', '1e30'), (3, '2025-01-01', 'c', '0.5'), (15, '-infinity', 'a', '1e30'), (-5, '1999-12-31', 'd', '0.5'), (1, '-infinity', 'a', NULL), (25, '2025-03-09', 'b', 'Infinity'), (19, '-infinity', 'c', 'NaN'), (18, 'infinity', 'a', NULL), (4, NULL, 'a', 'Infinity'), (-2, '2000-03-01', 'c', '123.45'), (16, '2025-03-09', 'c', NULL), (-4, '2000-03-01', 'd', '1e30'), (4, NULL, NULL, '1.23456789012345678901234567890');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "sv-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, NULL, 'ss', '2.5', '2147483647 days', 'empty', '{}', 'the and of', ARRAY[ROW(4, NULL)::r5ct]), (2, 2, '2', '-99999999999999999999999999999999999.999999', '-infinity', '[1,5)', '{[2,4)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(3, 'å')::r5ct, ROW(NULL, 'i')::r5ct, ROW(-2, 'ä')::r5ct]), (3, 19, 'Z', 'NaN', '-infinity', 'empty', '{[1,2],[2,3)}', 'the and of', ARRAY[ROW(NULL, NULL)::r5ct, ROW(1, NULL)::r5ct]), (4, -3, 'Z', '1e100', 'infinity', '[3,)', '{[1,3),[5,7)}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(2, '2')::r5ct]), (5, NULL, 'ab', NULL, '2147483647 days', '(,)', '{}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(3, 'ab')::r5ct, ROW(-2, 'ı')::r5ct, ROW(NULL, 'a-')::r5ct]), (6, 5, 'a', 'Infinity', '30 days', 'empty', '{empty}', 'the and of', ARRAY[ROW(0, 'I')::r5ct, ROW(NULL, NULL)::r5ct, ROW(2, 'E')::r5ct]), (7, NULL, 'c', '2.5', 'infinity', NULL, '{}', NULL, ARRAY[ROW(1, NULL)::r5ct]), (8, 9, 'h', '-0.5', '-infinity', 'empty', '{[1,2],[2,3)}', 'running runners ran; the runner runs', ARRAY[ROW(1, 'ch')::r5ct, ROW(-2, '10')::r5ct]), (9, 1, 'é', '0', '-1 mon 30 days', '[5,6)', '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', NULL), (10, NULL, 'Ab', '1e-130', NULL, NULL, '{[0,100)}', '', NULL);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -2 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 30 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (10, 'a', '2025-01-01'), (12, 'b', '2025-01-01') ON CONFLICT DO NOTHING;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob 1', 1, NULL, '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'ann2', 2, '{}', '[1, {"a": null}]'), (3, 'u3@x.org', 'zoë3', 3, '{}', '[1, {"a": null}]'), (4, 'u4@x.org', 'ann4', 4, NULL, '{"seats": 1.50}'), (5, 'u5@x.org', 'Éva5', 0, NULL, '{"seats": 1.50}'), (6, 'u6@x.org', 'zoë6', 0, NULL, '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (5, NULL, 'paid', '2025-02-02', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 12345678.90, 'void', '2025-02-08', '{"gift": true}'), (3, 9.99, 'paid', '2025-02-17', '{"items": [{"sku": "a", "qty": 2}]}'), (2, -1.50, 'new', '2025-02-25', NULL), (1, 0, 'void', '2025-02-16', '{"items": []}'), (4, NULL, 'paid', '2025-02-06', '{"gift": true}'), (5, 12345678.90, 'new', '2025-02-17', NULL);

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

INSERT INTO r7k VALUES (1, 1, -11, 'NOTE 3', 1000, 2.25, '2025-03-09 07:00:00+00'), (2, 1, 82, 'note 1', 3.333, 0.5, '2024-02-29 12:00:00+00'), (3, 3, 150, '', NULL, 0.5, '2024-02-29 12:00:00+00'), (4, 1, -47, 'é', NULL, 'NaN', '2025-03-09 07:00:00+00'), (5, 3, 120, 'note 2', -12.125, 'NaN', '2024-02-29 12:00:00+00'), (6, 7, 40, 'note_4', 3.333, 'NaN', '2025-03-09 07:00:00+00'), (7, 1, 33, 'Note 3', 1000, 1e300, '2025-03-09 07:00:00+00'), (8, 9, NULL, 'note_4', 0.5, 1e300, '2025-03-09 06:59:59+00'), (9, 1, 54, 'n%5', 0.5, 'Infinity', NULL), (10, 1, 91, 'NOTE 3', 1000, 2.25, '2025-03-09 06:59:59+00'), (11, 1, 119, 'é', NULL, NULL, NULL), (12, 1, 10, 'é', 3.333, 0.5, NULL), (13, 1, -8, 'note_4', 0.5, 'Infinity', '2024-02-29 12:00:00+00'), (14, 1, -44, 'note_4', 3.333, 2.25, '2025-11-02 05:30:00+00'), (15, 5, 56, 'note 2', 1000, 'NaN', '2025-11-02 05:30:00+00'), (16, 1, NULL, 'note 1', -12.125, NULL, NULL), (17, 5, 90, 'Note 3', 0.5, 1e300, '2024-02-29 12:00:00+00'), (18, 1, -5, 'note 2', 0, 2.25, '2024-02-29 12:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19);

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

INSERT INTO r7t VALUES (1, 'a', 'a'), (2, 'é', 'é'), (3, 'e', 'e'), (4, 'z', 'z'), (5, 'z', 'z'), (6, 'a', 'a'), (7, 'E', 'E'), (8, 'ä', 'ä'), (9, 'Æ', 'Æ'), (10, 'e', 'e');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, NULL, 1.50, 'z', '2025-01-01'::timestamp + interval '144 hours'), (2, 3, 16, 1.50, 'a', '2025-01-01'::timestamp + interval '9 hours'), (3, 3, 9, 0, 'ab', '2025-01-01'::timestamp + interval '2 hours'), (4, 3, 5, -2.25, 'a', '2025-01-01'::timestamp + interval '63 hours'), (5, 3, -2, NULL, 'a', '2025-01-01'::timestamp + interval '23 hours'), (6, 3, 11, 10, 'ab', '2025-01-01'::timestamp + interval '147 hours'), (7, 3, 14, 1.50, 'B', '2025-01-01'::timestamp + interval '44 hours'), (8, 3, 8, 1.50, 'ab', '2025-01-01'::timestamp + interval '78 hours'), (9, 4, 14, 1.50, '', '2025-01-01'::timestamp + interval '54 hours'), (10, 2, NULL, -2.25, 'b', '2025-01-01'::timestamp + interval '4 hours'), (11, 2, 12, 10, 'a', '2025-01-01'::timestamp + interval '48 hours'), (12, 3, 9, 3.75, 'b', '2025-01-01'::timestamp + interval '144 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 7, 1), (3, 8, 3), (3, 2, 6), (4, 3, 9), (4, 4, 5), (5, 5, 8), (5, 1, 4), (6, 3, 5), (6, 3, 6), (7, 6, 6), (7, 4, 2), (8, 5, 4), (8, 1, 7), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[8,)', '{}', '[2025-03-01,infinity)', NULL), (2, '[4,)', '{(,4)}', NULL, '[0,0]'), (3, NULL, NULL, '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (4, '[2,)', '{(,2)}', '[2025-01-01,2025-02-01)', '[0,0]'), (5, '(2,4]', '{}', 'empty', NULL), (6, '(1,7]', '{}', NULL, '[1.5,2.5)'), (7, NULL, NULL, '[2025-03-01,infinity)', '[1.5,2.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'foxes and dogs'), (2, 'über café naïve'), (3, 'über café naïve'), (4, 'jumps over the lazy dog'), (5, 'dog-eat-dog world'), (6, 'foxes and dogs');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (5, '"scalar"'), (6, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3931) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2791) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3554) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a44418 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a44418 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4402, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4402) i;

CREATE STATISTICS r9a44418_st ON g, (u % 10) FROM r9a44418;

ANALYZE r9a44418;

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a44418$$);
