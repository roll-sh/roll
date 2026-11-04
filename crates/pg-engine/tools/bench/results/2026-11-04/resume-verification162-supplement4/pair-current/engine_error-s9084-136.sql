-- class: engine_error
-- ordered: false
-- note: 26000: prepared statement "r9p86022" does not exist
-- engine: ERROR 26000: prepared statement "r9p86022" does not exist
-- reference: id:23 | x:1700
-- reference: 778 | 111.1429
-- reference: 777 | 111.0000
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a bool, b date, c int4);

INSERT INTO t0 (id, a, b, c) VALUES (25, NULL, '2024-01-05', NULL), (29, NULL, '2024-01-18', 2), (11, false, NULL, NULL), (9, NULL, '2024-01-22', 2), (31, NULL, NULL, -1), (16, NULL, NULL, 0), (34, NULL, '2024-03-17', -1), (21, NULL, '2024-03-04', NULL), (20, NULL, NULL, -1), (14, true, NULL, 4), (13, NULL, '2024-02-17', 7), (24, false, '2024-01-03', 8);

CREATE INDEX t0_i20 ON t0 (id) WHERE id IS NOT NULL;

CREATE INDEX t0_i722 ON t0 ((c + 1));

CREATE TABLE t1 (id int4 PRIMARY KEY, a interval, b d3text, c bool, d json, e timestamp);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (33, '1 mon', 'é', false, 'null', '2024-01-02 23:00:00');

CREATE TABLE t2 (a json, b text DEFAULT 0);

CREATE TYPE ct3 AS (a text COLLATE "C", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('ß', 3)::ct3, ARRAY[ROW('ab', 1)::ct3, ROW('ab', 3)::ct3], 3), (2, NULL, ARRAY[ROW('ba', 2)::ct3, ROW('Ab', 0)::ct3], 0), (3, ROW('bar', 0)::ct3, ARRAY[ROW('Ab', NULL)::ct3, ROW('a,b', 1)::ct3], 2), (4, ROW('a_b', 2)::ct3, ARRAY[ROW('Ä', 3)::ct3, ROW('b', 1)::ct3], 3), (5, ROW(NULL, 1)::ct3, NULL, 2), (6, NULL, NULL, 4), (7, ROW('Z', 0)::ct3, ARRAY[ROW(NULL, 0)::ct3, ROW('9', 3)::ct3], 1), (8, ROW(' x', 2)::ct3, ARRAY[ROW('9', 2)::ct3, ROW('Z', 0)::ct3], 1), (9, ROW(NULL, -1)::ct3, NULL, 0), (10, ROW('ab', 3)::ct3, ARRAY[ROW('bar', -1)::ct3, ROW('foo', 3)::ct3], 0), (11, NULL, ARRAY[ROW('', -1)::ct3, ROW('é', 3)::ct3], 1), (12, NULL, ARRAY[ROW('9', -1)::ct3, ROW(NULL, -1)::ct3], 2);

CREATE INDEX tc3_c ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE STRICT AS $$ SELECT CASE WHEN ('{"b": [1, "a"]}'::jsonb::jsonb ->> 'a')::int <= (SELECT count(*) FROM t2 t2 WHERE false) THEN p ELSE cardinality(array_remove('{NULL,3}'::int4[], p)) END $$;

CREATE VIEW v0 AS SELECT COALESCE(NULL::text[], string_to_array((s2.v0 || t1.b), ',')) AS c0 FROM (t1 t1 JOIN (VALUES (trim(t1.b)), ('ß'), (NULL), ('a_b')) s2(v0) ON t1.b = s2.v0) WHERE (trunc(12345678.9::numeric) * ceil('NaN'::numeric)) NOT IN (SELECT mod(mod(round(-0.25::numeric, 0), 3), 3) FROM t0 t3);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '2025-12-31 23:59:59.999999', '-infinity', '1 year 2 mons', '-2.5', NULL, 7), (2, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '-infinity', '2025-03-09 02:30:00', '4713-01-01 BC', 'infinity', NULL, '0.5', 8), (3, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '0044-03-15 12:00:00+00 BC', NULL, 'infinity', '23:59:59.999999', NULL, '1024', 4), (4, NULL, NULL, '2025-11-02 01:30:00-04', '1970-01-01 00:00:00.5', '4713-01-01 BC', 'P1DT12H', '99999999999999999999', '-1.25', 5), (5, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2025-10-26 02:30:00', '1999-12-31', '-12:00', '-2.5', '0.5', 2);

INSERT INTO tp4 VALUES (13, '4713-01-01 BC', 'c', 'NaN'), (18, '2024-02-29', 'c', '0'), (9, '4713-01-01 BC', 'd', '-7'), (16, NULL, 'b', '0.000001');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "pg_c_utf8", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 14, 'å', '0', '-infinity', NULL, '{[0,100)}', NULL, ARRAY[ROW(NULL, 'ı')::r5ct, ROW(-2, 'é')::r5ct, ROW(-2, 'e')::r5ct]), (2, 8, 'ch', NULL, '178000000 years', '[-3,0]', '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(3, NULL)::r5ct, ROW(NULL, 'B')::r5ct, ROW(4, 'I')::r5ct]), (3, 2, NULL, '7', '-infinity', '[0,10)', '{empty}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(-1, 'SS')::r5ct, ROW(2, 'z')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_b ON r5a (b COLLATE "POSIX");

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 28 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (1, 'a', 'infinity'), (1, 'b', NULL), (16, 'c', '2025-01-01'), (3, 'd', '2024-02-29'), (8, 'a', '2024-02-29'), (10, 'b', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (24, 'x0', NULL), (32, 'x1', NULL), (28, 'x2', NULL), (28, 'x3', NULL), (32, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', '1', 1, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'zoë2', 2, '{a}', '{"plan": "free", "tags": [1, 2]}'), (3, 'u3@x.org', 'ann3', 3, NULL, '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, 100, 'paid', '2025-02-14', NULL), (3, 9.99, 'paid', '2025-02-27', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, NULL, 'é', NULL, 1e300, NULL), (2, 1, 121, '', -12.125, 2.25, '2025-03-09 06:59:59+00'), (3, 1, 124, 'é', 1000, 1e300, '2025-11-02 05:30:00+00'), (4, 1, -25, 'é', 1000, -0, '2025-03-09 06:59:59+00'), (5, 1, 141, 'note 2', 3.333, 1e300, '2025-03-09 06:59:59+00'), (6, 1, 39, 'note 3', 3.333, 'NaN', '2025-03-09 06:59:59+00'), (7, 8, 74, 'NOTE 3', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (8, 1, 104, '', 3.333, 'NaN', '2025-03-09 07:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

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

INSERT INTO r7t VALUES (1, 'e', 'e'), (2, '9', '9'), (3, 'E', 'E'), (4, 'ä', 'ä'), (5, 'A', 'A'), (6, 'ä', 'ä');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 3, -4, 3.75, 'b', '2025-01-01'::timestamp + interval '134 hours'), (2, 2, 14, NULL, '', '2025-01-01'::timestamp + interval '21 hours'), (3, 4, NULL, 10, 'ab', '2025-01-01'::timestamp + interval '86 hours'), (4, 1, 0, 1.50, NULL, '2025-01-01'::timestamp + interval '45 hours'), (5, 4, 11, 0, NULL, '2025-01-01'::timestamp + interval '114 hours'), (6, 2, NULL, 1.50, 'B', '2025-01-01'::timestamp + interval '36 hours'), (7, 1, 4, NULL, 'a', '2025-01-01'::timestamp + interval '177 hours'), (8, 4, -5, 1.50, 'b', '2025-01-01'::timestamp + interval '186 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 3, 1), (1, 2, 9), (2, 3, 7), (3, 1, 3), (4, 1, 2), (4, 4, 4), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', NULL, 'empty', '(,0]'), (2, '[10,)', '{[10,11), [13, 15)}', 'empty', '[0,0]'), (3, '[5,)', '{[5,8), [10, 12)}', '[2025-01-01,2025-02-01)', NULL), (4, 'empty', NULL, '[2025-01-01,2025-02-01)', '(,0]'), (5, 'empty', '{}', 'empty', '[0,0]'), (6, '[6,)', '{}', '[2025-03-01,infinity)', '(1.25,1.5)'), (7, 'empty', '{(,-1)}', '[2025-03-01,infinity)', NULL);

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, ''), (2, 'dog-eat-dog world'), (3, 'brown''s fox''s den'), (4, 'running rats'), (5, 'quick quick slow'), (6, 'brown''s fox''s den'), (7, 'jumps over the lazy dog'), (8, 'foxes and dogs');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '"scalar"'), (4, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (6, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3365) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1941) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2401) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r9i68253 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i68253 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 225) i;

CREATE INDEX r9i68253_x0 ON r9i68253 USING gist (r) WHERE NOT isempty(r);

SET enable_seqscan = on;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i68253 WHERE to_tsvector('simple', t) @@ 'zz1';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i68253;

CREATE TABLE r8c26372 (id int NOT NULL, a text, b int DEFAULT 42, c date);

BEGIN;

COPY r8c26372 (id, b, a, c) FROM STDIN (FORMAT text, REJECT_LIMIT 2, LOG_VERBOSITY silent, FREEZE);
1	multi word	zz	2025-01-31	extra
2	x	3
3	\D	\N	2025-02-29
4	multi word		infinity	extra
5	\N	99999999999	\N
\.

COMMIT;

COPY r8c26372 TO STDOUT (FORMAT binary, HEADER);

COPY r8c26372 FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 0);

DROP TABLE r8c26372;

PREPARE r9p38516 (int, int) AS SELECT count(*) FROM r9s WHERE g = $1 AND v > 10 / $2;

EXECUTE r9p38516(5, 7);

EXECUTE r9p38516(3, 7);

EXECUTE r9p38516(1, 1);

EXECUTE r9p38516(5, 2);

EXECUTE r9p38516(99, 2);

EXPLAIN (COSTS OFF) EXECUTE r9p38516(1, 7);

EXECUTE r9p38516(1, 7);

EXECUTE r9p38516(2, 2);

EXECUTE r9p38516(3, -1);

EXECUTE r9p38516(2, -1);

ANALYZE r9s;

EXPLAIN (COSTS OFF) EXECUTE r9p38516(8, 1);

EXECUTE r9p38516(8, 1);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p38516';

DEALLOCATE r9p38516;

RESET plan_cache_mode;

CREATE TABLE r9i70651 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i70651_1 PARTITION OF r9i70651 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i70651_2 PARTITION OF r9i70651 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i70651 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 74) i;

ANALYZE r9i70651;

CREATE INDEX ON r9i70651 USING hash (a);

CREATE INDEX r9i70651_x1 ON r9i70651 USING spgist (r);

CREATE INDEX r9i70651_x2 ON r9i70651 USING btree (t COLLATE "C");

REINDEX TABLE r9i70651;

SET enable_seqscan = off;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i70651;

EXPLAIN (COSTS OFF) SELECT id, v FROM r9s WHERE id BETWEEN 2984 AND 3083 ORDER BY id DESC LIMIT 6;

CREATE TABLE r9i91212 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i91212_1 PARTITION OF r9i91212 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i91212_2 PARTITION OF r9i91212 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i91212 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3060) i;

CREATE UNIQUE INDEX ON r9i91212 USING btree (a);

CREATE INDEX r9i91212_x1 ON r9i91212 USING btree (a) WHERE a > 50;

CREATE INDEX r9i91212_x2 ON r9i91212 USING gin (j jsonb_path_ops);

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i91212 WHERE j @? '$.tags[*] ? (@ == "x1")';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i91212;

\s2 SELECT pg_advisory_lock(6);

\s3 SELECT pg_try_advisory_lock(6), pg_try_advisory_lock_shared(6), pg_try_advisory_xact_lock(6);

\s3& SELECT pg_advisory_lock(6);

\s2 SELECT pg_advisory_unlock(6), pg_advisory_unlock(6);

\s3.;

\s3 SELECT locktype, mode, granted FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid();

\s3 SELECT pg_advisory_unlock_all();

\s2 SELECT pg_advisory_unlock_all();

\s2 ROLLBACK;

\s3 ROLLBACK;

SET work_mem = '64kB';

RESET work_mem;

CREATE TABLE r9c23357 (id int PRIMARY KEY, a int, b text) WITH (autovacuum_enabled = off);

CREATE UNIQUE INDEX r9c23357_b ON r9c23357 (b);

BEGIN;

COPY r9c23357 (id, a, b) FROM STDIN (FORMAT csv);
1,2,b1
2,9,b2
3,16,b3
4,23,b4
5,x1,b5
6,37,b6
7,44,b7
\.

\s2& INSERT INTO r9c23357 (id, a, b) VALUES (5, 0, 'w5');

ROLLBACK;

\s2.;

DROP TABLE r9c23357;

SET enable_indexonlyscan = off;

RESET enable_indexonlyscan;

SET plan_cache_mode = force_generic_plan;

PREPARE r9p6912 (text) AS SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;

EXECUTE r9p6912('{1,2,3}');

EXECUTE r9p6912('{1,2,3}');

EXECUTE r9p6912('{}');

EXECUTE r9p6912('{2999,1}');

EXECUTE r9p6912('{50,100,NULL}');

EXECUTE r9p6912('{}');

EXECUTE r9p6912('{}');

EXECUTE r9p6912('{50,100,NULL}');

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p6912';

DEALLOCATE r9p6912;

RESET plan_cache_mode;

\s2 BEGIN ISOLATION LEVEL SERIALIZABLE;

\s3 BEGIN ISOLATION LEVEL SERIALIZABLE;

\s2 SELECT sum(v) FROM r7k WHERE grp IN (1, 2);

\s3 SELECT sum(v) FROM r7k WHERE grp IN (1, 2);

\s2 UPDATE r7k SET v = coalesce(v, 0) + 1 WHERE id = (SELECT min(id) FROM r7k WHERE grp = 2);

\s3 UPDATE r7k SET v = coalesce(v, 0) + 1 WHERE id = (SELECT min(id) FROM r7k WHERE grp = 1);

\s2 COMMIT;

\s3 COMMIT;

\s2 ROLLBACK;

\s3 ROLLBACK;

PREPARE r9p86022 AS SELECT id, x FROM r9s WHERE id BETWEEN $1 AND $1 + $2 ORDER BY id DESC LIMIT 20;

-- statement
EXECUTE r9p86022(777, 1);
