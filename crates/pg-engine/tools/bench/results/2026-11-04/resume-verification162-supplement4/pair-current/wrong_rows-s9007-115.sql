-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a53362  (rows=1)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a53362  (rows=29)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a numeric(10,2), b date, c numeric(10,2));

INSERT INTO t0 (id, a, b, c) VALUES (17, NULL, '2024-02-27', -1.00), (21, 8.00, '2024-01-27', -1.00), (3, NULL, NULL, NULL), (30, 0.75, '2024-02-01', NULL), (4, NULL, '2024-03-15', 0.05), (23, NULL, NULL, 8.25), (6, 2.05, NULL, NULL), (40, NULL, '2024-02-11', -3.75), (37, -2.05, '2024-02-04', -1.75), (1, 8.25, '2024-02-11', 7.25), (39, NULL, '2024-01-23', 0.50), (25, 7.00, '2024-02-12', -1.00), (11, 5.05, NULL, NULL), (18, NULL, '2024-01-17', 7.75), (22, NULL, NULL, NULL), (46, 3.00, '2024-02-11', NULL), (36, NULL, NULL, NULL), (32, 12.00, '2024-03-05', 0.05), (16, 0.25, NULL, NULL), (47, -2.50, '2024-01-26', 3.25);

CREATE TABLE t1 (a int2, b text[], c jsonb);

INSERT INTO t1 (a, b, c) VALUES (3, '{z}', '{"a": {"b": 2}}'), (1, '{A,a}', NULL), (NULL, NULL, NULL), (2, '{z}', NULL), (1, NULL, NULL), (0, NULL, '{"a": {"b": 2}}'), (6, NULL, NULL), (2, NULL, '{"a": "1"}');

CREATE INDEX t1_i330 ON t1 (c);

CREATE INDEX t1_i768 ON t1 (b DESC NULLS LAST);

CREATE INDEX t1_i369 ON t1 (c);

CREATE VIEW v0 AS SELECT '-2 days'::interval AS c0, json_build_array(NULLIF((t2.c::jsonb ->> 'a')::int, (s1.v0 + 11)), '9') AS c1 FROM ((VALUES ((-2)), (5)) s1(v0) JOIN t1 t2 ON (false OR EXISTS (SELECT DISTINCT abs(NULL::int4) FROM t0 t3 WHERE (initcap('a,b') || upper(NULL::text)) <= (substr('x ', 2, 1) COLLATE "und-x-icu")))) WHERE (true OR (SELECT bool_or(t4.b >= t4.b) FROM t0 t4 WHERE false) < t2.b = t2.b);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2038-01-19 03:14:08+00', '0001-01-01 00:00:00 BC', NULL, '2 weeks 3 microseconds', '1.23456789012345678901234567890', '0.5', 12), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '-infinity', '2024-02-29 00:00:00', '1999-12-31', '-1 year -1 day', 'Infinity', NULL, 9), (3, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2025-03-09 01:30:00-05', 'infinity', '2024-02-29', '1 year 2 mons', '-1e-20', '2', 1), (4, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', NULL, '1970-01-01 00:00:00.5', 'infinity', 'P1DT12H', '2.5', '-0', -3), (5, NULL, NULL, '2025-11-02 01:30:00-05', '2025-11-02 01:30:00', '2000-03-01', NULL, '0', '0.125', 10), (6, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '2025-12-31 23:59:59.999999', '2025-03-09', 'infinity', '0.000001', '-1.25', 6), (7, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '2024-02-29 00:00:00', '2000-03-01', NULL, '-Infinity', '-3', -3);

INSERT INTO tp4 VALUES (23, '4713-01-01 BC', 'b', '-1e-20'), (21, '2025-01-01', 'a', NULL), (13, 'infinity', 'c', '1e30'), (17, '2025-01-01', 'b', '-1e-20'), (-1, '1999-12-31', 'd', '1e30'), (13, '2024-02-29', NULL, 'Infinity');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "ucs_basic", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 4, '', 'Infinity', NULL, '[3,)', NULL, 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(3, '2')::r5ct]), (2, -3, 'z', NULL, '2147483647 days', NULL, '{[2,4)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(2, NULL)::r5ct, ROW(2, 'c')::r5ct]), (3, -1, 'I', 'NaN', '0', '(1,5]', '{[0,100)}', 'Fat cats! FAT rats? fat-free', NULL), (4, NULL, 'a-', NULL, '2147483647 days', '(1,5]', '{[1,2],[2,3)}', 'Fat cats! FAT rats? fat-free', '{}'), (5, NULL, 'ß', '7', '-1 mon 30 days', '[-3,0]', '{empty}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(-1, 'ss')::r5ct]), (6, 2, 'E', 'NaN', NULL, '[1,5)', '{empty}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(NULL, 'I')::r5ct, ROW(NULL, 'B')::r5ct]), (7, NULL, 'a-', '-Infinity', 'infinity', '[3,)', NULL, 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE INDEX r5a_iv ON r5a (iv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 11 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (8, 'a', 'infinity'), (19, 'b', '2025-01-01'), (3, 'c', '2025-01-01'), (-3, 'd', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (30, 'x0', NULL), (28, 'x1', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 1, '{a}', '{}'), (2, 'u2@x.org', '2', 2, NULL, '{}'), (3, 'u3@x.org', 'bob3', 0, '{}', '{"seats": 1.50}'), (4, 'u4@x.org', 'Cid4', 4, '{b,NULL}', '{"seats": 1.50}'), (5, 'u5@x.org', '5', 5, NULL, NULL), (6, 'u6@x.org', 'zoë6', 0, NULL, '{"plan": "free", "tags": [1, 2]}'), (7, 'u7@x.org', 'Éva7', 7, '{a}', NULL), (8, 'u8@x.org', 'zoë8', 8, '{}', '{"plan": "pro", "seats": 3}'), (9, 'u9@x.org', 'Cid9', 9, '{a,b}', '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (4, NULL, 'void', '2025-02-23', '{"items": []}'), (8, 9.99, 'void', '2025-02-15', '{"items": []}'), (5, 100, 'new', '2025-02-24', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 100, 'void', '2025-02-04', '{"gift": true}'), (7, 0, 'paid', '2025-02-12', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 7, 49, 'Note 3', 0, NULL, '2024-02-29 12:00:00+00'), (2, 1, -21, 'é', 3.333, -0, NULL), (3, 4, 69, 'n%5', -12.125, NULL, '2025-11-02 05:30:00+00'), (4, 1, 190, 'note_4', -12.125, NULL, '2025-03-09 06:59:59+00'), (5, 1, 121, 'note 3', 3.333, 1e300, '2025-11-02 05:30:00+00'), (6, 1, -38, 'é', 3.333, 1e300, '2025-03-09 06:59:59+00'), (7, 1, NULL, 'note_4', 1000, -0, '2025-11-02 05:30:00+00'), (8, 1, NULL, 'note_4', 3.333, 1e300, '2025-03-09 07:00:00+00'), (9, 1, 174, 'Note 3', NULL, 2.25, '2025-03-09 06:59:59+00'), (10, 2, 53, 'note_4', 1000, 0.5, NULL), (11, 5, 53, 'note 1', 0.5, 'NaN', NULL), (12, 1, 29, 'n%5', 0.5, 1e300, '2025-03-09 07:00:00+00'), (13, 1, -40, 'n%5', 3.333, NULL, '2025-03-09 06:59:59+00'), (14, 1, 166, 'note 2', 3.333, 'Infinity', NULL), (15, 2, 71, 'NOTE 3', -12.125, 1e300, '2025-03-09 06:59:59+00'), (16, 9, 36, 'note_4', -12.125, 2.25, '2024-02-29 12:00:00+00'), (17, 9, 39, 'é', 0, NULL, NULL), (18, 4, 186, 'note 1', NULL, 0.5, '2025-03-09 06:59:59+00'), (19, 7, 188, 'Note 3', 1000, -0, NULL), (20, 1, 184, 'n%5', 0, 'NaN', '2024-02-29 12:00:00+00'), (21, 1, 35, 'n%5', -12.125, NULL, '2025-03-09 07:00:00+00'), (22, 7, 128, 'note 2', -12.125, NULL, '2024-02-29 12:00:00+00'), (23, 8, 2, 'note 2', 0, -0, '2025-03-09 06:59:59+00'), (24, 1, 150, 'n%5', NULL, -0, NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21);

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

INSERT INTO r7t VALUES (1, 'a', 'a'), (2, 'ä', 'ä'), (3, 'A', 'A'), (4, '', ''), (5, 'é', 'é'), (6, 'e', 'e'), (7, 'E', 'E'), (8, 'e', 'e'), (9, 'b', 'b'), (10, '10', '10'), (11, 'E', 'E'), (12, 'a', 'a'), (13, 'é', 'é');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, NULL, -2.25, 'a', '2025-01-01'::timestamp + interval '54 hours'), (2, 3, -5, 10, 'ab', '2025-01-01'::timestamp + interval '56 hours'), (3, 1, -3, 10, 'ab', '2025-01-01'::timestamp + interval '38 hours'), (4, 2, NULL, -2.25, 'B', '2025-01-01'::timestamp + interval '66 hours'), (5, 3, -5, 1.50, 'b', '2025-01-01'::timestamp + interval '60 hours'), (6, 4, 16, 1.50, 'ab', '2025-01-01'::timestamp + interval '50 hours'), (7, 2, 4, 1.50, 'z', '2025-01-01'::timestamp + interval '120 hours'), (8, 2, 6, 3.75, 'a', '2025-01-01'::timestamp + interval '86 hours'), (9, 4, 12, 1.50, 'a', '2025-01-01'::timestamp + interval '44 hours'), (10, 4, 0, NULL, 'z', '2025-01-01'::timestamp + interval '138 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 2, 7), (2, 5, 1), (3, 1, 3), (3, 3, 4), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{[9,12), [14, 16)}', '[2025-03-01,infinity)', '[0,0]'), (2, 'empty', '{}', '(2024-12-31,2025-01-05]', '(,0]'), (3, 'empty', '{[6,9), [11, 13)}', 'empty', '(1.25,1.5)'), (4, '(-2,0]', '{(,-2)}', NULL, '[0,0]'), (5, 'empty', '{}', 'empty', NULL), (6, 'empty', '{[9,15), [17, 19)}', '[2025-01-01,2025-02-01)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'the quick brown fox'), (2, 'dog-eat-dog world'), (3, 'dog-eat-dog world'), (4, 'a rat ran'), (5, 'über café naïve'), (6, 'dog-eat-dog world');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (4, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (5, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (6, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2766) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2487) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2298) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

UPDATE r6u SET name = '', version = version + 1 WHERE id = 1 AND version = 2 RETURNING *;

CREATE TABLE r9c43223 (id int, a int, b text) PARTITION BY RANGE (id);

CREATE TABLE r9c43223_1 PARTITION OF r9c43223 FOR VALUES FROM (MINVALUE) TO (500) WITH (autovacuum_enabled = off);

CREATE TABLE r9c43223_2 PARTITION OF r9c43223 FOR VALUES FROM (500) TO (1500) WITH (autovacuum_enabled = off);

CREATE TABLE r9c43223_3 PARTITION OF r9c43223 FOR VALUES FROM (1500) TO (2600) WITH (autovacuum_enabled = off);

CREATE UNIQUE INDEX r9c43223_id ON r9c43223 (id);

BEGIN;

COPY r9c43223 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
7	44	b7
8	1	b8
9	8	b9
10	15	b10
11	22	b11
12	29	b12
\.

\s2& INSERT INTO r9c43223 (id, a, b) VALUES (12, 0, 'w12') ON CONFLICT DO NOTHING;

COMMIT;

\s2.;

DROP TABLE r9c43223;

SET enable_sort = off;

RESET enable_sort;

CREATE PROCEDURE r8f34672(INOUT n int, m int) LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('proc', 'first ' || m);
  COMMIT AND CHAIN;
  INSERT INTO r8log (tg, info) VALUES ('proc', 'second ' || m);
  n := n + m;
END $$;

CALL r8f34672(1, 2);

BEGIN;

CALL r8f34672(5, 6);

ROLLBACK;

DO $$ DECLARE x int := 0; BEGIN CALL r8f34672(x, 3); RAISE NOTICE 'x=%', x; END $$;

DROP PROCEDURE r8f34672;

EXPLAIN (COSTS OFF) SELECT g, v, id FROM r9s WHERE (g, v) > (4, -52) ORDER BY g, v, id LIMIT 23;

CREATE TABLE r5p3 PARTITION OF r5p FOR VALUES FROM (30) TO (40);

INSERT INTO r7d (id, p, ps, code) VALUES (8 + 10, 100, '{3,NULL}', 'it''s') RETURNING *;

\s2 INSERT INTO r7d (id, p, ps, code) VALUES (-1 + 10, 42, '{}', 'it''s') RETURNING *;

\s3 INSERT INTO r7d (id, p, ps, code) VALUES (2 + 10, 2147483647, '{1,1,99}', $$z$$) RETURNING *;

\x2 name=r7n46013 types=23,23,1007,25 describe rows=1 p=b00000001 p=t'42' p=b0000000100000000000000170000000000000001 p=b4125
INSERT INTO r7d (id, p, ps, code) VALUES ($1 + 10, $2, $3, $4) RETURNING *;

\x2 name=r7n46013 rf=1 p=t'6' p=t'1' p=t'{}' p=b69742773
-;

\x2 name=r7n46013 p=t'0' p=t'-2147483648' p=b000000010000000000000017000000020000000100000004000000010000000400000002 p=b61
-;

\x2 name=r7n46013 p=t'0' p=t'42' p=t'{3,NULL}' p=t'a'
-;

\x2 name=r7n46013 rows=2 p=t'6' p=t'1' p=t'{1,1,99}' p=t'NULL'
-;

\x2 name=r7n46013 rf=1 p=t'11' p=t'100' p=t'{1,1,99}' p=t'A%'
-;

\x2 name=r7n46013 p=t'-1' p=t'1' p=t'{1,1,99}' p=t'z'
-;

\x2 name=r7n46013 p=t'99' p=t'7' p=t'{}' p=t''
-;

\s2 DEALLOCATE r7n46013;

SELECT numnode(to_tsquery('simple', 'jump:*')), querytree(to_tsquery('simple', 'jump:*')), to_tsquery('simple', 'jump:*') && 'x'::tsquery, !! 'a'::tsquery, 'a'::tsquery <-> 'b <-> c'::tsquery, tsquery_phrase('a', 'b', 3), ts_rewrite('a & b'::tsquery, 'a'::tsquery, 'c | d'::tsquery), 'fat:ab & cat'::tsquery @> 'cat'::tsquery;

PREPARE r9p98002 AS SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

EXECUTE r9p98002(7);

EXECUTE r9p98002(-1);

EXECUTE r9p98002(NULL);

EXECUTE r9p98002(-50);

EXECUTE r9p98002(0);

EXECUTE r9p98002(NULL);

EXECUTE r9p98002(-50);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p98002';

DEALLOCATE r9p98002;

RESET plan_cache_mode;

ALTER TABLE r5a ALTER COLUMN e2 SET DEFAULT 1.5;

CREATE FUNCTION r8ef98186() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'end % %', tg_tag, (SELECT string_agg(command_tag || ':' || object_type || ':' || coalesce(object_identity, '-') || ':' || in_extension, ', ' ORDER BY object_identity, command_tag) FROM pg_event_trigger_ddl_commands()); END $$;

CREATE EVENT TRIGGER r8ef98186 ON ddl_command_end EXECUTE FUNCTION r8ef98186();

CREATE TABLE r8e98186 (a int PRIMARY KEY, b text);

CREATE INDEX r8e98186_b ON r8e98186 (b);

ALTER TABLE r8e98186 ALTER COLUMN a TYPE bigint;

CREATE VIEW r8e98186_v AS SELECT a FROM r8e98186;

ALTER TABLE r8e98186 DROP COLUMN b;

DROP TABLE r8e98186 CASCADE;

SELECT evtname, evtevent, evtenabled, evttags FROM pg_event_trigger WHERE evtname = 'r8ef98186';

DROP EVENT TRIGGER r8ef98186;

DROP FUNCTION r8ef98186;

SELECT range_merge('{}'::int4multirange);

WITH RECURSIVE p(n) AS (SELECT 1 INTERSECT SELECT n + 1 FROM p) CYCLE n SET c USING path SELECT * FROM p;

BEGIN;

DECLARE r5k CURSOR FOR SELECT k, v FROM r5p ORDER BY k, v;

FETCH 2 FROM r5k;

SAVEPOINT r5s1;

INSERT INTO r5a (id, a) VALUES (50, 50);

ROLLBACK TO r5s1;

MOVE 1 IN r5k;

FETCH RELATIVE 1 FROM r5k;

COMMIT AND CHAIN;

FETCH ALL FROM r5k;

COMMIT;

CLOSE ALL;

\s2 BEGIN;

\s2 UPDATE r8a SET v = coalesce(v, 0) + 100 WHERE id = 4 RETURNING id, v;

\s2 INSERT INTO r8log (tg, info) VALUES ('2pc', 'r8gx94497');

\s2 PREPARE TRANSACTION 'r8gx94497';

SELECT gid, owner, database = current_database() AS here FROM pg_prepared_xacts ORDER BY gid;

\s2 PREPARE TRANSACTION 'r8gx94497';

SELECT 1;

ROLLBACK;

\s2 ROLLBACK;

ROLLBACK PREPARED 'r8gx94497';

\s3.;

COMMIT PREPARED 'r8gx94497';

SELECT count(*) FROM pg_prepared_xacts WHERE gid = 'r8gx94497';

SELECT numnode(to_tsquery('simple', 'jump:*')), querytree(to_tsquery('simple', 'jump:*')), to_tsquery('simple', 'jump:*') && 'x'::tsquery, !! 'a'::tsquery, 'a'::tsquery <-> 'b <-> c'::tsquery, tsquery_phrase('a', 'b', 3), ts_rewrite('a & b'::tsquery, 'a'::tsquery, 'c | d'::tsquery), 'fat:ab & cat'::tsquery @> 'cat'::tsquery;

CREATE TABLE r9a53362 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a53362 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 87, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 87) i;

CREATE STATISTICS r9a53362_st (mcv) ON g, s FROM r9a53362;

ANALYZE r9a53362 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a53362 WHERE g = 3$$);

SELECT * FROM r9est($$SELECT g, s FROM r9a53362 GROUP BY g, s$$);

EXPLAIN SELECT g, s FROM r9a53362 GROUP BY g, s;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a53362 WHERE arr @> '{2}'$$);
