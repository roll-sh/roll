-- class: wrong_rows
-- ordered: false
-- note: 6 vs 6 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Sort
-- engine:         Sort Key: (t) COLLATE "en-x-icu", id
-- engine:         ->  Seq Scan on r9s
-- engine:               Disabled: true
-- engine:               Filter: (((t)::text >= 'a'::text) AND ((t)::text <= 'b'::text))
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: t COLLATE "en-x-icu", id
-- reference:         ->  Seq Scan on r9s
-- reference:               Disabled: true
-- reference:               Filter: (((t)::text >= 'a'::text) AND ((t)::text <= 'b'::text))
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a json, b int4[], c bool, d text[], e jsonb);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (5, NULL, '{}', NULL, NULL, NULL), (21, '"s"', NULL, NULL, NULL, NULL), (40, NULL, NULL, NULL, NULL, '[]'), (14, NULL, '{NULL,3}', NULL, NULL, NULL), (50, '{"b": [1, "a"]}', NULL, false, NULL, NULL), (28, NULL, '{}', false, NULL, '"s"'), (36, '{"a": 3}', NULL, false, NULL, NULL), (58, '"s"', NULL, NULL, '{z}', NULL), (24, NULL, NULL, NULL, NULL, NULL), (29, NULL, NULL, false, '{}', NULL), (26, '{"a": 1, "b": "x"}', NULL, NULL, NULL, '[1, 2, 3]'), (7, NULL, '{1,2}', false, '{ab}', NULL), (37, NULL, NULL, NULL, NULL, NULL), (27, NULL, NULL, NULL, NULL, NULL), (8, '{"a": 3}', NULL, false, NULL, NULL), (35, NULL, NULL, NULL, NULL, NULL), (22, NULL, NULL, true, NULL, NULL), (54, NULL, NULL, NULL, NULL, NULL), (1, NULL, '{1,2}', false, NULL, NULL), (59, NULL, NULL, NULL, NULL, NULL);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (a interval, b int4, c d3int, d d3text, e bool, g int GENERATED ALWAYS AS (coalesce(b, 0) * 2 + 1) VIRTUAL) PARTITION BY HASH (b);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES WITH (MODULUS 3, REMAINDER 0);

CREATE TABLE t1_p1 PARTITION OF t1 FOR VALUES WITH (MODULUS 3, REMAINDER 1);

CREATE TABLE t1_p2 PARTITION OF t1 FOR VALUES WITH (MODULUS 3, REMAINDER 2);

INSERT INTO t1 (a, b, c, d, e) VALUES ('2 hours', 3, 1, '10', false);

CREATE TABLE t2 (id int4 PRIMARY KEY, a jsonb, b jsonb, c numeric(10,2), d numeric(10,2), g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int4[], b bool, c text, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t3 (id, a, b, c) VALUES (40, NULL, true, 'ba');

CREATE INDEX t3_i890 ON t3 USING hash (a);

CREATE INDEX t3_i519 ON t3 USING hash (g);

CREATE INDEX t3_i961 ON t3 (g);

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('aa', 2)::ct3, NULL, NULL), (2, ROW(NULL, 2)::ct3, ARRAY[ROW('x ', 1)::ct3, ROW('a,b', 2)::ct3], NULL), (3, NULL, ARRAY[ROW('A', 0)::ct3, ROW('a', 0)::ct3], 3), (4, ROW('ba', NULL)::ct3, NULL, NULL), (5, ROW('ba', 0)::ct3, ARRAY[ROW('b', 1)::ct3, ROW('A', 0)::ct3], 4), (6, ROW('é', 1)::ct3, '{}', 2), (7, ROW(NULL, 2)::ct3, '{}', 2), (8, ROW('bar', 3)::ct3, ARRAY[ROW(' x', 0)::ct3, ROW('aa', 1)::ct3], NULL), (9, ROW('9', -1)::ct3, '{}', 0);

CREATE INDEX tc3_ca ON tc3 (ca);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE STRICT AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN false THEN RETURN COALESCE(cardinality('{NULL,3}'::int4[]), array_length('{1,1,2}'::int4[], 1));
  ELSE RETURN p;
  END CASE;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN ('{1,2}'::int4[] || '{3,1,2}'::int4[]) @> ARRAY[p, p] THEN p ELSE cardinality(('{5}'::int4[] || NULL::int4[])) END $$;

CREATE VIEW v0 AS SELECT cardinality(('{NULL,3}'::int4[] || array_remove('{1,1,2}'::int4[], 3))) AS c0, t2.a AS c1 FROM (unnest('{5}'::int4[]) WITH ORDINALITY s1(u, o) JOIN t2 t2 ON s1.u = t2.id AND s1.u < t2.g AND true) WHERE width_bucket(t2.c, 0, 10, 4) IS NOT DISTINCT FROM f1(f1(t2.g, ' x'), (NULL::text || 'ba'));

CREATE VIEW v1 AS SELECT sum(NULLIF(('NaN'::numeric + 'NaN'::numeric), NULLIF(99.99::numeric, -0.25::numeric))) AS c0, (('{}'::jsonb)::text::jsonb || NULLIF('{"a": 2.50}'::jsonb, '{"b": [1, "a"]}'::jsonb)) AS c1, min((t2.a)[1]) AS c2, (substr(NULL::text, 2, 3) COLLATE "default") AS c3 FROM (generate_series(1, 1) s1(g) JOIN t3 t2 ON s1.g = t2.g) WHERE t2.b;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{}', '{}', NULL, '2025-10-26 02:30:00', '2000-03-01', '1 day', '1e30', NULL, 0), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', '2025-12-31 23:59:59.999999', '2025-03-09', '-infinity', 'Infinity', NULL, 1), (3, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2025-03-09 01:30:00-05', 'infinity', '2000-03-01', '0', NULL, '-3', 11), (4, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2025-03-09 02:30:00', '-infinity', 'infinity', 'NaN', NULL, 12), (5, '"str"', '"str"', '2024-02-29 23:00:00+00', '1970-01-01 00:00:00.5', '2024-02-29', '-1 mon 2 days', '-1e-20', '1024', NULL), (6, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-03-09 03:30:00-04', 'infinity', '4713-01-01 BC', '36 hours', '-1e-20', '2', 11), (7, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-11-02 01:30:00-04', '2025-03-09 02:30:00', '4713-01-01 BC', '0', '0.5', '0.5', 12);

INSERT INTO tp4 VALUES (23, '2024-02-29', 'a', '0.000001'), (13, '2025-03-09', 'ab', '-2.5'), (13, '2024-02-29', 'c', '99999999999999999999');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "en-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 17, '10', NULL, '30 days', 'empty', NULL, 'The quick brown fox jumps over the lazy dog', NULL), (2, NULL, 'I', '0', NULL, '(,)', '{}', '', ARRAY[ROW(5, 'e')::r5ct]), (3, 11, 'i', '-99999999999999999999999999999999999.999999', '1 day 00:00:00.000001', '[-3,0]', '{[1,2],[2,3)}', '', ARRAY[ROW(1, 'b')::r5ct, ROW(NULL, 'a-')::r5ct, ROW(-2, 'ä')::r5ct]), (4, -1, 'å', '0', '2147483647 mons', '(1,5]', '{[0,100)}', '', ARRAY[ROW(2, 'ß')::r5ct, ROW(NULL, NULL)::r5ct, ROW(1, 'o')::r5ct]), (5, NULL, '-a', '0.0000000000000000000000000001', '0', NULL, '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-1, 'h')::r5ct, ROW(NULL, NULL)::r5ct, ROW(NULL, 'Ab')::r5ct]), (6, NULL, '2', 'Infinity', NULL, '(,2]', '{(,0],[10,)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(NULL, 'ı')::r5ct]), (7, NULL, 'ß', '1e100', '2147483647 days', '(,)', '{}', 'PostgreSQL full text search: lexemes & weights', '{}'), (8, -2, 'ae', '-0.5', '-1 mon 30 days', '[1,5)', '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-2, 'Ä')::r5ct, ROW(5, 'A')::r5ct]), (9, 2, 'ss', 'NaN', '30 days', '(1,5]', NULL, 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(4, 'b')::r5ct, ROW(2, 'ä')::r5ct]), (10, NULL, 'ı', '7', '720:00:00', '(,)', NULL, 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(2, NULL)::r5ct]);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 11 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (11, 'a', 'infinity'), (16, 'b', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 0, '{}', '{}'), (2, 'u2@x.org', 'ann2', 2, '{}', '[1, {"a": null}]'), (3, 'u3@x.org', 'Cid3', 0, NULL, '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, -1.50, 'paid', '2025-02-21', '{"gift": true}'), (3, 0, 'paid', '2025-02-28', NULL), (2, NULL, 'void', '2025-02-20', '{"gift": true}'), (3, NULL, 'paid', '2025-02-23', '{"items": []}'), (1, -1.50, 'new', '2025-02-12', '{"items": []}'), (2, 0, 'paid', '2025-02-04', '{"items": []}'), (1, 9.99, 'paid', '2025-02-04', '{"gift": true}'), (2, 0, 'new', '2025-02-13', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 12345678.90, 'void', '2025-02-06', NULL), (1, 12345678.90, 'void', '2025-02-28', NULL), (1, 9.99, 'void', '2025-02-17', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 2, 6, 'note 1', 0.5, 0.5, '2024-02-29 12:00:00+00'), (2, 1, 84, 'Note 3', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (3, 1, 96, 'n%5', 1000, 2.25, '2025-11-02 05:30:00+00'), (4, 1, -15, 'note 2', -12.125, 2.25, '2025-03-09 06:59:59+00'), (5, 4, -30, 'note 1', 3.333, 'Infinity', '2025-03-09 07:00:00+00'), (6, 1, 136, 'n%5', 0.5, 'Infinity', '2025-03-09 07:00:00+00'), (7, 1, 198, 'Note 3', 1000, NULL, '2025-03-09 07:00:00+00'), (8, 1, 138, 'Note 3', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (9, 7, -21, 'note 3', -12.125, NULL, '2025-11-02 05:30:00+00'), (10, 2, -9, 'n%5', 0, 2.25, '2025-03-09 07:00:00+00'), (11, 1, NULL, 'é', NULL, 2.25, '2025-11-02 05:30:00+00'), (12, 8, -11, 'n%5', 1000, -0, NULL), (13, 1, -40, 'Note 3', 1000, 0.5, NULL), (14, 1, NULL, 'Note 3', 0.5, 'Infinity', '2025-03-09 07:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12);

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

INSERT INTO r7t VALUES (1, '9', '9'), (2, 'z', 'z'), (3, 'é', 'é'), (4, '', ''), (5, '9', '9'), (6, 'b', 'b'), (7, 'b', 'b'), (8, '9', '9'), (9, 'b', 'b'), (10, 'z', 'z');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

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

INSERT INTO r8a VALUES (1, 2, -1, 1.50, 'b', '2025-01-01'::timestamp + interval '100 hours'), (2, 1, 2, 10, 'B', '2025-01-01'::timestamp + interval '30 hours'), (3, 3, 12, -2.25, '', '2025-01-01'::timestamp + interval '17 hours'), (4, 1, -2, -2.25, '', '2025-01-01'::timestamp + interval '72 hours'), (5, 4, NULL, 10, 'z', '2025-01-01'::timestamp + interval '39 hours'), (6, 4, 20, 1.50, '', '2025-01-01'::timestamp + interval '42 hours'), (7, 4, -1, 1.50, 'B', '2025-01-01'::timestamp + interval '156 hours'), (8, 4, 10, 10, 'z', '2025-01-01'::timestamp + interval '27 hours'), (9, 2, 1, -2.25, 'a', '2025-01-01'::timestamp + interval '80 hours'), (10, 4, 9, 1.50, 'z', '2025-01-01'::timestamp + interval '12 hours'), (11, 2, NULL, 10, '', '2025-01-01'::timestamp + interval '120 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 8), (3, 2, 1), (3, 3, 4), (4, 2, 3), (4, 3, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[3,8)', '{[3,8), [10, 12)}', '[2025-03-01,infinity)', '(,0]'), (2, 'empty', NULL, NULL, '[0,0]'), (3, '[1,2)', NULL, '(2024-12-31,2025-01-05]', '[0,0]'), (4, NULL, '{(,-3)}', NULL, '(,0]'), (5, 'empty', '{}', NULL, '(,0]'), (6, 'empty', '{[2,7), [9, 11)}', '[2025-03-01,infinity)', '(1.25,1.5)'), (7, 'empty', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (8, '[1,2)', '{(,1)}', '(2024-12-31,2025-01-05]', NULL), (9, '(0,4]', '{(,0)}', '[2025-03-01,infinity)', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, ''), (3, 'foxes and dogs'), (4, 'running rats'), (5, 'foxes and dogs'), (6, 'dog-eat-dog world'), (7, 'a rat ran'), (8, 'dog-eat-dog world'), (9, 'running rats');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2412) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2003) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3188) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

VACUUM r9s;

SET enable_seqscan = off;

-- statement
EXPLAIN (COSTS OFF) SELECT id, t FROM r9s WHERE t COLLATE "en-x-icu" BETWEEN 'a' AND 'b' ORDER BY t COLLATE "en-x-icu", id LIMIT 4;
