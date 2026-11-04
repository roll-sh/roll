-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a87187  (rows=10)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a87187  (rows=642)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a text, b text, c timestamptz, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b, c) VALUES (19, 'abc', NULL, '2024-01-07 06:00:00+00');

CREATE INDEX t0_i136 ON t0 (b);

CREATE INDEX t0_i337 ON t0 (a);

CREATE INDEX t0_i460 ON t0 (b, id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a int4, b float8, c text, d json, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t1 (id, a, b, c, d) VALUES (5, NULL, -1.75, NULL, '{"a": 3}'), (6, NULL, NULL, '9', '{"a": 3}'), (24, NULL, NULL, NULL, NULL), (14, NULL, 2.75, NULL, '[1, 2, 3]'), (17, NULL, -1.75, NULL, NULL), (29, 4, 0.25, 'a,b', NULL), (22, NULL, 0.5, NULL, '{"a": 1, "b": "x"}'), (4, NULL, 1.75, NULL, '{"a": 1, "b": "x"}');

CREATE INDEX t1_i194 ON t1 (c);

CREATE INDEX t1_i875 ON t1 USING hash (c);

CREATE TYPE ct3 AS (a text, b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('Z', 1)::ct3, '{}', NULL), (2, NULL, NULL, 3), (3, ROW('a,b', -1)::ct3, NULL, NULL), (4, ROW('Ab', 2)::ct3, ARRAY[ROW('b', 2)::ct3, ROW('z', NULL)::ct3], 1), (5, ROW('x ', 3)::ct3, ARRAY[ROW('Ab', 3)::ct3, ROW(NULL, NULL)::ct3], 2), (6, ROW(NULL, 1)::ct3, '{}', 3), (7, ROW('', 2)::ct3, '{}', NULL), (8, ROW('', 3)::ct3, ARRAY[ROW('abc', NULL)::ct3, ROW(NULL, -1)::ct3], NULL), (9, ROW('Z', 2)::ct3, ARRAY[ROW('b', 1)::ct3, ROW('ß', 3)::ct3], 1);

ANALYZE;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, a FROM t0 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, a FROM t0 WHERE id IS NULL;
  END IF;
END
$$;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '"str"', '"str"', '1999-12-31 23:59:59.999999+00', '2024-02-29 00:00:00', 'infinity', '-infinity', NULL, '1024', 1), (2, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2025-11-02 01:30:00', '2025-03-09', '1 year 2 mons', '-Infinity', '1024', 2), (3, '"str"', '"str"', '2025-10-26 00:30:00+00', NULL, NULL, NULL, NULL, 'Infinity', 5), (4, '"str"', '"str"', '2025-04-06 02:30:00+11', NULL, '2025-01-01', '23:59:59.999999', '99999999999999999999', NULL, -3);

INSERT INTO tp4 VALUES (1, '2025-03-09', 'ab', '-7'), (9, '4713-01-01 BC', 'b', '99999999999999999999'), (11, '2024-02-29', 'a', '-7'), (19, NULL, 'b', '2.5'), (18, '2025-01-01', NULL, '0.5'), (16, '2000-03-01', 'b', '0'), (24, NULL, 'ab', '123.45'), (0, '2025-03-09', 'ab', NULL), (16, '2024-02-29', NULL, '-0.0'), (-3, '2000-03-01', 'd', '-1e-20'), (15, '4713-01-01 BC', 'd', '1e30'), (18, '2025-03-09', 'c', '99999999999999999999'), (21, NULL, 'b', NULL), (18, '2000-03-01', 'a', '-7'), (-4, '2025-03-09', 'b', '0.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5ai", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 6, 'å', 'Infinity', '-infinity', '[1,5)', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(2, 'c')::r5ct, ROW(3, 'ı')::r5ct]), (2, -3, 'ab', '-Infinity', '-infinity', NULL, '{}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(4, 'a')::r5ct, ROW(3, NULL)::r5ct, ROW(NULL, 'B')::r5ct]), (3, 2, 'c', '7', '178000000 years', 'empty', '{}', 'PostgreSQL full text search: lexemes & weights', NULL), (4, 2, 'ı', '1e-130', '-2147483648 days', '(,2]', '{empty}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(3, 'ch')::r5ct]), (5, 14, 'z', '1e100', '720:00:00', 'empty', '{}', NULL, NULL), (6, -3, 'İ', '1e-130', '-1 mon 30 days', '[3,)', '{[1,3),[5,7)}', 'running runners ran; the runner runs', ARRAY[ROW(2, 'b')::r5ct, ROW(2, 'A')::r5ct, ROW(1, 'a-')::r5ct]), (7, 7, 'İ', '7', NULL, '(,2]', '{}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-2, 'ss')::r5ct, ROW(2, 'I')::r5ct]), (8, 8, 'å', '123456789012345678901234567890.123456789', '-infinity', '[-3,0]', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(-2, 'B')::r5ct, ROW(4, ' a')::r5ct, ROW(5, 'é')::r5ct]);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (NEW.g1 > 0) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 30 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (1, 'a', 'infinity'), (10, 'b', '2024-02-29'), (18, 'c', '2024-02-29'), (10, 'd', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL), (31, 'x1', NULL), (22, 'x2', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 0, '{a}', '{}'), (2, 'u2@x.org', 'bob2', 2, '{a}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'Cid3', 0, '{b,NULL}', '{"seats": 1.50}'), (4, 'u4@x.org', 'dee4', 0, '{a}', '[1, {"a": null}]'), (5, 'u5@x.org', 'ann5', 5, '{a}', NULL), (6, 'u6@x.org', 'bob6', 6, '{a,b}', '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (5, NULL, 'paid', '2025-02-11', '{"items": []}'), (6, NULL, 'new', '2025-02-28', NULL), (6, NULL, 'paid', '2025-02-14', '{"items": []}'), (6, 100, 'void', '2025-02-26', '{"items": []}'), (4, 9.99, 'new', '2025-02-08', '{"items": []}'), (4, NULL, 'void', '2025-02-19', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 100, 'new', '2025-02-15', '{"items": []}'), (2, 12345678.90, 'paid', '2025-02-24', '{"items": []}'), (5, 9.99, 'paid', '2025-02-08', NULL), (3, NULL, 'void', '2025-02-04', '{"items": []}'), (2, 9.99, 'void', '2025-02-18', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 100, 'paid', '2025-02-15', '{"gift": true}'), (6, 100, 'void', '2025-02-22', '{"gift": true}'), (5, NULL, 'paid', '2025-02-12', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 9.99, 'paid', '2025-02-05', NULL), (3, 9.99, 'paid', '2025-02-03', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 0, 'new', '2025-02-14', NULL), (4, NULL, 'new', '2025-02-19', '{"gift": true}'), (3, -1.50, 'void', '2025-02-28', '{"gift": true}'), (1, -1.50, 'new', '2025-02-04', NULL), (3, NULL, 'void', '2025-02-27', '{"items": []}'), (5, 9.99, 'new', '2025-02-23', '{"gift": true}'), (6, 100, 'new', '2025-02-27', '{"items": [{"sku": "a", "qty": 2}]}');

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

INSERT INTO r7k VALUES (1, 1, 33, 'note 3', -12.125, NULL, '2024-02-29 12:00:00+00'), (2, 1, 145, 'Note 3', NULL, 'Infinity', '2025-03-09 07:00:00+00'), (3, 1, 75, 'Note 3', 3.333, -0, NULL), (4, 1, NULL, 'note 3', -12.125, NULL, '2025-11-02 05:30:00+00'), (5, 1, NULL, 'note 2', 1000, 'NaN', '2025-03-09 07:00:00+00'), (6, 7, -27, 'Note 3', 1000, 'NaN', '2025-03-09 07:00:00+00'), (7, 1, 176, 'note 1', 0.5, NULL, NULL), (8, 9, NULL, '', -12.125, -0, '2025-03-09 06:59:59+00'), (9, 1, 149, 'Note 3', NULL, 0.5, '2025-03-09 06:59:59+00'), (10, 1, -4, 'é', 0, -0, '2025-03-09 06:59:59+00'), (11, 1, 35, 'note 1', NULL, 0.5, '2025-11-02 05:30:00+00'), (12, 1, -19, 'note 1', -12.125, 1e300, '2024-02-29 12:00:00+00'), (13, 3, -40, 'note 2', 1000, 2.25, '2024-02-29 12:00:00+00'), (14, 4, 83, 'n%5', 0, 1e300, '2025-03-09 07:00:00+00'), (15, 6, 135, 'NOTE 3', 3.333, 'Infinity', NULL), (16, 2, -20, 'note_4', 1000, 2.25, NULL), (17, 8, 15, 'n%5', 0, 1e300, '2025-03-09 06:59:59+00'), (18, 4, -36, 'n%5', -12.125, -0, '2025-03-09 06:59:59+00'), (19, 1, 97, 'note_4', 0.5, 'Infinity', '2025-11-02 05:30:00+00'), (20, 6, 13, 'note 3', 3.333, 1e300, '2025-03-09 06:59:59+00'), (21, 1, 145, 'note 3', 1000, 1e300, '2025-11-02 05:30:00+00'), (22, 1, 16, 'note_4', -12.125, NULL, '2025-03-09 07:00:00+00'), (23, 1, NULL, 'note 1', 0, 'Infinity', NULL), (24, 8, 82, 'note 3', 0.5, 1e300, '2025-11-02 05:30:00+00'), (25, 2, -35, '', NULL, -0, '2025-03-09 06:59:59+00'), (26, 1, 29, 'note 2', 0.5, -0, '2025-11-02 05:30:00+00'), (27, 1, -40, '', 0, 'Infinity', NULL), (28, 1, 67, 'note_4', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (29, 3, 126, 'note_4', 3.333, 0.5, '2025-11-02 05:30:00+00'), (30, 6, 18, 'n%5', 1000, 'Infinity', '2025-03-09 06:59:59+00'), (31, 1, 103, 'NOTE 3', 0, 2.25, '2025-11-02 05:30:00+00'), (32, 1, 136, 'note 2', NULL, 1e300, '2024-02-29 12:00:00+00'), (33, 3, -10, 'NOTE 3', 3.333, 'Infinity', NULL), (34, 1, NULL, 'note 2', 3.333, 2.25, '2025-03-09 06:59:59+00'), (35, 3, 71, 'n%5', 1000, 0.5, '2025-03-09 07:00:00+00'), (36, 1, 73, '', -12.125, 1e300, '2024-02-29 12:00:00+00'), (37, 1, 194, 'note 1', 0.5, 'NaN', NULL), (38, 7, 168, 'note 1', 3.333, 'Infinity', NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19);

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

INSERT INTO r7t VALUES (1, 'ae', 'ae'), (2, 'b', 'b'), (3, 'A', 'A'), (4, '10', '10'), (5, 'a', 'a'), (6, '', ''), (7, '', '');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

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

INSERT INTO r8a VALUES (1, 4, 4, 3.75, NULL, '2025-01-01'::timestamp + interval '96 hours'), (2, 2, 13, 3.75, 'ab', '2025-01-01'::timestamp + interval '112 hours'), (3, 1, 18, 0, 'b', '2025-01-01'::timestamp + interval '0 hours'), (4, 2, 19, 10, 'b', '2025-01-01'::timestamp + interval '37 hours'), (5, 1, 5, 1.50, 'b', '2025-01-01'::timestamp + interval '195 hours'), (6, 1, 12, -2.25, NULL, '2025-01-01'::timestamp + interval '42 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (4, 6, 9), (4, 4, 9), (5, 5, 6), (6, 6, 8), (6, 1, 8), (8, 7, 6), (8, 3, 3), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[0,)', NULL, NULL, '[0,0]'), (2, '(6,6]', NULL, NULL, '(1.25,1.5)'), (3, NULL, '{(,5)}', NULL, '(1.25,1.5)'), (4, 'empty', '{(,6)}', 'empty', '[1.5,2.5)'), (5, 'empty', '{(,8)}', 'empty', '[0,0]'), (6, NULL, '{}', NULL, '(,0]'), (7, '[8,13)', NULL, '[2025-03-01,infinity)', '[1.5,2.5)'), (8, 'empty', '{[-2,3), [5, 7)}', '(2024-12-31,2025-01-05]', NULL), (9, 'empty', '{(,8)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, ''), (3, 'the quick brown fox'), (4, 'foxes and dogs'), (5, 'the quick brown fox'), (6, 'brown''s fox''s den'), (7, 'brown''s fox''s den'), (8, 'running rats');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '"scalar"'), (4, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (6, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2604) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2627) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2149) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9i31674 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i31674 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3483) i;

CREATE INDEX CONCURRENTLY r9i31674_x0 ON r9i31674 USING gin (arr);

INSERT INTO r9i31674 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(3484, 3556) i;

UPDATE r9i31674 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i31674 WHERE id % 11 = 0;

REINDEX TABLE r9i31674;

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i31674 ORDER BY p <-> point '(3,3)', id LIMIT 2;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i31674;

CREATE TABLE r9a87187 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a87187 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1927, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1927) i;

ALTER TABLE r9a87187 ALTER COLUMN arr SET STATISTICS 13;

ANALYZE r9a87187 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a87187 WHERE nul IS NULL$$);

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a87187 WHERE arr @> '{2}'$$);
