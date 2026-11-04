-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: QUERY PLAN:25
-- engine: Seq Scan on r9a38793  (cost=0.00..79.71 rows=15 width=78)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Seq Scan on r9a38793  (cost=0.00..79.71 rows=979 width=78)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a numeric(10,2), b float8, c d3int, d text, e date, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED) PARTITION BY RANGE (c);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t0_p1 PARTITION OF t0 FOR VALUES FROM (3) TO (6);

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b, c, d, e) VALUES (19, -1.00, -0.5, NULL, 'ß', NULL), (34, 4.00, NULL, 2, 'a%', '2024-02-17'), (9, -1.00, 3.5, NULL, NULL, '2024-02-01'), (17, NULL, -2, NULL, 'bar', '2024-03-21'), (21, 2.50, NULL, 5, 'B', '2024-02-23'), (2, 4.75, -1.5, NULL, 'abc', NULL), (40, -3.75, -0.75, 7, 'ba', NULL), (14, -1.75, 2.75, 8, 'z', NULL), (3, 10.75, NULL, NULL, 'foo', NULL), (43, 7.05, NULL, NULL, NULL, NULL), (36, NULL, NULL, NULL, ' x', NULL), (33, -2.75, -2, 0, NULL, NULL);

CREATE INDEX t0_i723 ON t0 (b) WHERE b IS NOT NULL;

CREATE INDEX t0_i992 ON t0 (g);

CREATE INDEX t0_i34 ON t0 (d) WHERE d IS NOT NULL;

CREATE TABLE t1 (id int4 PRIMARY KEY, a float8 DEFAULT 0.25, b numeric(10,2) DEFAULT 8.75, c int8, d bool, e interval);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (12, 0, NULL, NULL, true, NULL), (30, 2.5, NULL, NULL, false, NULL), (15, NULL, NULL, -1, NULL, NULL), (24, -0.5, 1.75, NULL, true, '1 day'), (2, NULL, 11.00, NULL, NULL, NULL), (32, -0.75, NULL, NULL, NULL, NULL), (1, NULL, NULL, NULL, true, '1 mon'), (5, NULL, NULL, NULL, NULL, NULL), (46, -0.5, 6.00, NULL, NULL, NULL), (26, NULL, NULL, NULL, NULL, '1 day'), (36, NULL, NULL, NULL, false, '30 days'), (56, NULL, NULL, NULL, false, NULL), (6, NULL, NULL, 12, NULL, NULL), (4, 1, NULL, 0, NULL, '2 hours'), (25, -0.5, NULL, 12, false, NULL), (29, NULL, NULL, NULL, false, NULL), (3, 4, NULL, NULL, NULL, NULL), (40, NULL, NULL, NULL, NULL, NULL), (17, NULL, NULL, 20, NULL, NULL), (14, NULL, NULL, NULL, NULL, NULL), (22, -1.5, NULL, NULL, true, '24:00:00'), (19, NULL, 0.25, 4, NULL, NULL), (34, NULL, 11.50, 9, NULL, NULL), (31, NULL, 4.50, 7, NULL, '0'), (11, NULL, NULL, NULL, NULL, '30 days'), (42, NULL, 8.75, NULL, NULL, NULL), (10, 3.5, NULL, -2, NULL, NULL), (27, NULL, NULL, -1, true, NULL), (52, NULL, NULL, 6, NULL, NULL), (8, 2, NULL, NULL, false, NULL);

CREATE INDEX t1_i588 ON t1 (d);

CREATE INDEX t1_i935 ON t1 (c, id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t2 (id int4 PRIMARY KEY, a float8 DEFAULT 1.25, b interval, c interval);

INSERT INTO t2 (id, a, b, c) VALUES (3, NULL, NULL, '1 mon');

CREATE INDEX t2_i458 ON t2 (id, c);

CREATE INDEX t2_i815 ON t2 (c);

CREATE TYPE ct3 AS (a text, b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('foo', NULL)::ct3, '{}', NULL), (2, ROW(NULL, 3)::ct3, '{}', 1), (3, NULL, NULL, NULL), (4, ROW('é', 1)::ct3, NULL, 1);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', NULL, '2025-01-01', '1 day', NULL, '0.5', 3), (2, '"str"', '"str"', '0044-03-15 12:00:00+00 BC', NULL, '-infinity', '-12:00', '-2.5', NULL, -3), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '1999-12-31 23:59:59.999999+00', '2000-01-01 00:00:00', '2025-03-09', '0', NULL, '-1.25', 9), (4, NULL, NULL, '2025-10-26 01:30:00+00', '2025-10-26 02:30:00', '1999-12-31', '23:59:59.999999', '2.5', '2', -1), (5, NULL, NULL, '0044-03-15 12:00:00+00 BC', NULL, '-infinity', '0', '0.000001', '-0', NULL), (6, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-11-02 01:30:00-04', '0001-01-01 00:00:00 BC', '2024-02-29', '-1 mon 2 days', '-0.0', NULL, 2), (7, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '-infinity', 'infinity', '4713-01-01 BC', '-12:00', '0.5', NULL, 11), (8, '{}', '{}', '2025-10-26 01:30:00+00', '2025-12-31 23:59:59.999999', '1999-12-31', '-1 mon 2 days', 'Infinity', NULL, 6);

INSERT INTO tp4 VALUES (20, '2024-02-29', NULL, '2.5'), (14, 'infinity', NULL, NULL), (24, '2025-01-01', 'a', '-2.5'), (3, 'infinity', NULL, '0.000001'), (25, '2025-03-09', 'd', '1e30'), (25, '2024-02-29', 'ab', NULL), (21, '2025-03-09', 'a', '0.000001'), (11, '2000-03-01', 'ab', '-Infinity'), (21, 'infinity', 'b', '99999999999999999999'), (-2, '2000-03-01', 'c', '0.5'), (5, '2024-02-29', 'c', NULL), (18, 'infinity', 'd', 'Infinity'), (-3, '4713-01-01 BC', 'ab', '0.000001'), (14, '4713-01-01 BC', 'b', '0');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "tr-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 17, 'E', '7', '-2147483648 days', '[1,5)', '{(,0],[10,)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL), (2, 6, '-a', '-Infinity', '2147483647 mons', '(,)', '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', NULL), (3, 0, 'é', '-99999999999999999999999999999999999.999999', '720:00:00', '[3,)', '{}', '', NULL), (4, 5, 'I', NULL, NULL, 'empty', '{[1,2],[2,3)}', 'Supernovae stars are the brightest phenomena in galaxies', '{}'), (5, 1, 'b', '2.5', '1 mon', '(,)', '{empty}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(0, NULL)::r5ct, ROW(-2, NULL)::r5ct, ROW(0, 'z')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 23 WITH LOCAL CHECK OPTION;

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

INSERT INTO r5p VALUES (1, 'a', '2024-02-29'), (2, 'b', NULL), (-5, 'c', NULL), (17, 'd', '2024-02-29') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (22, 'x0', NULL), (29, 'x1', NULL), (24, 'x2', NULL), (30, 'x3', NULL), (32, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 1, '{}', '{"seats": 1.50}'), (2, 'u2@x.org', 'bob 2', 2, '{a,b}', '[1, {"a": null}]'), (3, 'u3@x.org', 'ann3', 3, '{a}', '{"seats": 1.50}'), (4, 'u4@x.org', 'ann4', 0, '{a,b}', '{"plan": "pro", "seats": 3}'), (5, 'u5@x.org', 'Éva5', 5, '{}', NULL), (6, 'u6@x.org', 'zoë6', 6, '{a}', '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'bob7', 7, NULL, '{"seats": 1.50}'), (8, 'u8@x.org', 'dee8', 8, '{b,NULL}', '{}'), (9, 'u9@x.org', 'Éva9', 9, NULL, '{"plan": "free", "tags": [1, 2]}'), (10, 'u10@x.org', 'ann10', 10, NULL, NULL), (11, 'u11@x.org', 'Cid11', 11, '{}', '{"plan": "free", "tags": [1, 2]}'), (12, 'u12@x.org', 'Éva12', 0, '{a,b}', '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (2, 0, 'new', '2025-02-16', NULL), (10, 0, 'new', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (6, 0, 'paid', '2025-02-24', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 6, 39, 'Note 3', 1000, 2.25, NULL), (2, 1, 164, 'note 3', NULL, 2.25, '2025-03-09 06:59:59+00'), (3, 1, 118, 'Note 3', 0.5, 0.5, '2024-02-29 12:00:00+00'), (4, 1, 44, 'note_4', 0, 'NaN', '2025-03-09 07:00:00+00'), (5, 8, 133, 'note 3', 1000, 2.25, '2025-03-09 06:59:59+00'), (6, 1, 118, 'Note 3', 3.333, -0, '2025-11-02 05:30:00+00'), (7, 1, 85, 'note 1', 0.5, 0.5, NULL), (8, 1, 38, 'note 3', 1000, NULL, NULL), (9, 1, -24, '', 0.5, -0, '2025-03-09 06:59:59+00'), (10, 1, 100, 'note 1', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (11, 1, NULL, 'note_4', 0, 0.5, '2025-03-09 07:00:00+00'), (12, 1, 44, 'é', NULL, 0.5, '2025-11-02 05:30:00+00'), (13, 1, 51, '', 0, 0.5, '2025-11-02 05:30:00+00'), (14, 1, 32, 'Note 3', 0.5, 2.25, '2025-11-02 05:30:00+00'), (15, 1, -18, 'note 3', 0.5, 1e300, NULL), (16, 1, 67, 'note_4', 3.333, NULL, '2025-03-09 07:00:00+00'), (17, 1, 105, 'note_4', 1000, -0, NULL), (18, 1, -14, 'n%5', 0, 'Infinity', '2025-03-09 06:59:59+00'), (19, 1, 137, 'note 3', 1000, 0.5, '2025-03-09 07:00:00+00'), (20, 9, -24, 'note 2', 0, -0, '2025-03-09 06:59:59+00'), (21, 5, NULL, 'Note 3', 0, 'NaN', NULL), (22, 1, 120, 'note 3', -12.125, 0.5, '2025-03-09 07:00:00+00'), (23, 1, 66, 'note 1', 0.5, 1e300, '2025-03-09 07:00:00+00'), (24, 1, 178, 'note 3', 3.333, -0, '2025-11-02 05:30:00+00'), (25, 1, 0, 'note 3', 0, NULL, '2025-03-09 06:59:59+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

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

INSERT INTO r7t VALUES (1, 'z', 'z'), (2, '', ''), (3, 'ß', 'ß'), (4, 'A', 'A'), (5, '10', '10'), (6, 'Ä', 'Ä'), (7, 'ae', 'ae'), (8, 'ss', 'ss'), (9, 'e', 'e'), (10, 'ae', 'ae'), (11, 'ae', 'ae');

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

GRANT SELECT (id, grp) ON r7k TO r7_alice;

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

INSERT INTO r8a VALUES (1, 2, NULL, 1.50, 'z', '2025-01-01'::timestamp + interval '90 hours'), (2, 1, 9, -2.25, 'B', '2025-01-01'::timestamp + interval '58 hours'), (3, 1, NULL, 10, 'a', '2025-01-01'::timestamp + interval '18 hours'), (4, 3, 9, 1.50, 'z', '2025-01-01'::timestamp + interval '45 hours'), (5, 1, 16, 0, 'z', '2025-01-01'::timestamp + interval '26 hours'), (6, 3, -2, 3.75, 'z', '2025-01-01'::timestamp + interval '2 hours'), (7, 4, NULL, -2.25, NULL, '2025-01-01'::timestamp + interval '34 hours'), (8, 2, 9, 3.75, 'z', '2025-01-01'::timestamp + interval '36 hours'), (9, 2, 11, 1.50, NULL, '2025-01-01'::timestamp + interval '82 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 4, 7), (3, 1, 3), (3, 1, 8), (4, 4, 9), (4, 4, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{[9,12), [14, 16)}', '[2025-03-01,infinity)', '(,0]'), (2, 'empty', '{}', '[2025-03-01,infinity)', '[0,0]'), (3, 'empty', '{}', NULL, '(1.25,1.5)'), (4, 'empty', '{(,4)}', '[2025-03-01,infinity)', '(1.25,1.5)'), (5, '[-3,)', '{(,-3)}', '[2025-03-01,infinity)', NULL), (6, '(-2,-1]', NULL, '[2025-01-01,2025-02-01)', NULL);

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, 'jumps over the lazy dog'), (3, 'the quick brown fox'), (4, 'quick quick slow');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2188) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2687) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3231) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a33271 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a33271 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 5978, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 5978) i;

ANALYZE r9a33271;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a33271 WHERE s = 'common'$$);

EXPLAIN SELECT * FROM r9a33271 WHERE s = 'common';

SELECT * FROM r9est($$SELECT * FROM r9a33271 WHERE nul IS NULL$$);

SELECT * FROM r9est($$SELECT * FROM r9a33271 WHERE g = 2 AND s = 'common'$$);

DROP TABLE r9a33271;

SET enable_indexscan = off;

RESET enable_indexscan;

CREATE TABLE r9w73343 (a int, b text);

INSERT INTO r9w73343 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q73343 AS SELECT a FROM r9w73343 WHERE a >= $1 ORDER BY a;

EXECUTE r9q73343(1);

EXECUTE r9q73343(2);

ALTER TABLE r9w73343 RENAME COLUMN b TO bb;

EXECUTE r9q73343(1);

DEALLOCATE r9q73343;

DROP TABLE r9w73343;

CREATE TABLE r9a38793 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a38793 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2937, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2937) i;

ALTER TABLE r9a38793 ALTER COLUMN g SET STATISTICS 100;

CREATE STATISTICS r9a38793_st (ndistinct) ON g, (u % 10) FROM r9a38793;

ANALYZE (SKIP_LOCKED) r9a38793;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a38793 WHERE arr @> '{2}'$$);

-- statement
EXPLAIN SELECT * FROM r9a38793 WHERE arr @> '{2}';
