-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a52922  (rows=23)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a52922  (rows=1524)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a text[], b date, c text);

INSERT INTO t0 (id, a, b, c) VALUES (31, '{}', '2024-01-03', 'z'), (14, NULL, '2024-01-06', '10'), (41, NULL, '2024-03-22', 'A'), (10, '{a,b}', '2024-03-25', 'bar'), (1, '{a,b}', '2024-01-16', NULL), (7, '{b,NULL}', '2024-01-14', 'abc'), (19, '{}', NULL, '10'), (40, '{A,a}', '2024-03-08', 'bar'), (26, '{A,a}', '2024-01-23', 'aa'), (3, '{ab}', '2024-03-17', 'foo'), (16, '{b,NULL}', '2024-02-21', 'x '), (35, NULL, '2024-02-07', 'a,b'), (37, '{A,a}', '2024-03-11', 'z'), (36, '{}', '2024-01-27', 'B'), (24, '{z}', '2024-02-27', 'Z'), (5, NULL, NULL, 'x '), (13, '{z}', NULL, 'Ab'), (8, '{ab}', '2024-02-26', 'B'), (12, '{a,b}', NULL, 'a%'), (32, '{}', '2024-03-13', NULL);

CREATE INDEX t0_i579 ON t0 USING hash (c);

CREATE INDEX t0_i819 ON t0 (c);

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamp, b text[]);

INSERT INTO t1 (id, a, b) VALUES (20, '2024-01-02 06:00:00', '{A,a}');

CREATE INDEX t1_i963 ON t1 (b);

CREATE TYPE ct3 AS (a text COLLATE "ci3", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('foo', NULL)::ct3, ARRAY[ROW(NULL, 2)::ct3, ROW('abc', 0)::ct3], NULL), (2, NULL, ARRAY[ROW('a,b', 3)::ct3, ROW(NULL, -1)::ct3], 4), (3, NULL, '{}', 3), (4, ROW('Ab', 1)::ct3, ARRAY[ROW('x ', 3)::ct3, ROW(NULL, -1)::ct3], NULL), (5, ROW('a', 0)::ct3, NULL, NULL), (6, ROW('bar', 1)::ct3, ARRAY[ROW('ba', 1)::ct3, ROW(NULL, 3)::ct3], 0), (7, ROW('a%', 2)::ct3, ARRAY[ROW('abc', 1)::ct3, ROW('ß', -1)::ct3], 4), (8, ROW('a', -1)::ct3, NULL, 0), (9, ROW('10', 2)::ct3, NULL, 4), (10, ROW(NULL, NULL)::ct3, '{}', NULL), (11, ROW('Ä', 3)::ct3, '{}', 2), (12, ROW('x ', 1)::ct3, ARRAY[ROW('bar', 0)::ct3, ROW(NULL, NULL)::ct3], 0), (13, ROW('a', 0)::ct3, ARRAY[ROW('9', 1)::ct3, ROW('Z', 1)::ct3], 1);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_ca ON tc3 (ca);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN false THEN p ELSE length(q) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN false THEN extract(day from ('2024-12-11'::date - p))::int ELSE p END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, c FROM t0 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, c FROM t0 WHERE id IS NULL;
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

INSERT INTO tj4 VALUES (1, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-03-09 03:30:00-04', '2000-01-01 00:00:00', '2000-03-01', '-1 year -1 day', '-7', '1024', 5), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-03-09 03:30:00-04', '2025-11-02 01:30:00', '2024-02-29', '0', '2.5', '-3', 7), (3, NULL, NULL, NULL, '0001-01-01 00:00:00 BC', '2024-02-29', '-infinity', NULL, '-1.25', 3), (4, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', '2024-02-29 00:00:00', 'infinity', '-12:00', NULL, 'Infinity', 4), (5, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '0044-03-15 12:00:00+00 BC', '2025-10-26 02:30:00', '1999-12-31', '1 mon -30 days', '1.23456789012345678901234567890', '0.125', 0), (6, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '2025-12-31 23:59:59.999999', '1999-12-31', '2 weeks 3 microseconds', '-Infinity', 'Infinity', 0), (7, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '0001-01-01 00:00:00 BC', '2025-01-01', NULL, '1.23456789012345678901234567890', '-3', 3), (8, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '0001-01-01 00:00:00 BC', '4713-01-01 BC', 'P1DT12H', '-0.0', '1024', NULL), (9, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '2025-10-26 02:30:00', '1999-12-31', '1 mon -30 days', '0.5', '1024', NULL);

INSERT INTO tp4 VALUES (-3, '1999-12-31', NULL, '-2.5'), (16, '-infinity', 'd', '-1e-20'), (12, '4713-01-01 BC', 'c', '-7'), (13, NULL, 'ab', '99999999999999999999'), (14, '2025-01-01', 'c', 'Infinity');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "unicode", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 20, 'a-', '-0.5', '1 day 00:00:00.000001', '(,2]', NULL, 'Fat cats! FAT rats? fat-free', ARRAY[ROW(4, 'B')::r5ct, ROW(1, '-a')::r5ct]), (2, 5, 'i', NULL, '2147483647 mons', NULL, '{(,0],[10,)}', 'running runners ran; the runner runs', '{}'), (3, 6, 'a', NULL, '178000000 years', '(,)', '{[1,3),[5,7)}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(0, 'ı')::r5ct, ROW(NULL, 'İ')::r5ct, ROW(NULL, '')::r5ct]), (4, 6, 'ae', NULL, '-infinity', NULL, '{empty}', 'running runners ran; the runner runs', ARRAY[ROW(2, 'SS')::r5ct]), (5, 17, 'İ', '-Infinity', '2147483647 days', '[-3,0]', '{(,0],[10,)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(-2, 'ä')::r5ct, ROW(2, 'ß')::r5ct, ROW(5, 'é')::r5ct]), (6, 13, 'ä', '0.0000000000000000000000000001', '2147483647 days', '(,)', '{[1,2],[2,3)}', 'running runners ran; the runner runs', '{}'), (7, -2, 'A', 'Infinity', '1 year -1 sec', '[-3,0]', NULL, '', ARRAY[ROW(1, 'AB')::r5ct]);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0 WITH CHECK OPTION;

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

INSERT INTO r5p VALUES (-5, 'a', '2024-02-29'), (8, 'b', 'infinity'), (3, 'c', NULL), (15, 'd', '2024-02-29'), (10, 'a', '2025-01-01'), (15, 'b', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (20, 'x0', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'zoë1', 0, '{}', '{}'), (2, 'u2@x.org', 'zoë2', 0, '{b,NULL}', '{"seats": 1.50}'), (3, 'u3@x.org', 'Éva3', 3, '{a}', '{}'), (4, 'u4@x.org', 'ann4', 4, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (5, 'u5@x.org', '5', 0, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (6, 'u6@x.org', 'zoë6', 0, '{a,b}', '{"seats": 1.50}'), (7, 'u7@x.org', '7', 0, '{}', '{"plan": "free", "tags": [1, 2]}'), (8, 'u8@x.org', 'bob8', 8, '{a,b}', '{"seats": 1.50}'), (9, 'u9@x.org', 'dee9', 0, '{}', '{"plan": "free", "tags": [1, 2]}'), (10, 'u10@x.org', 'bob 10', 10, NULL, '{"seats": 1.50}'), (11, 'u11@x.org', '11', 0, NULL, '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (2, 100, 'new', '2025-02-03', '{"items": [{"sku": "a", "qty": 2}]}'), (5, -1.50, 'new', '2025-02-13', '{"gift": true}'), (11, -1.50, 'void', '2025-02-18', NULL), (7, NULL, 'void', '2025-02-17', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 7, NULL, 'Note 3', NULL, -0, NULL), (2, 1, 200, 'NOTE 3', 3.333, 'Infinity', NULL), (3, 1, 53, 'NOTE 3', 0, 'Infinity', '2025-03-09 07:00:00+00'), (4, 8, 157, 'n%5', 0, -0, '2025-03-09 06:59:59+00'), (5, 3, 200, 'n%5', NULL, -0, '2025-03-09 07:00:00+00'), (6, 6, 100, '', 0, 'NaN', NULL), (7, 1, 174, 'é', NULL, 0.5, NULL), (8, 1, NULL, 'é', 3.333, 1e300, '2025-03-09 07:00:00+00'), (9, 1, 20, '', 0.5, NULL, '2025-03-09 06:59:59+00'), (10, 1, 35, 'note 2', 1000, 0.5, NULL), (11, 1, 64, 'NOTE 3', NULL, 0.5, '2025-03-09 07:00:00+00'), (12, 1, NULL, 'note 1', NULL, 'NaN', '2025-11-02 05:30:00+00'), (13, 9, 124, 'note 1', 1000, 2.25, '2025-03-09 06:59:59+00'), (14, 1, 125, 'Note 3', NULL, 'NaN', NULL), (15, 1, NULL, 'note 3', 0.5, 'Infinity', '2024-02-29 12:00:00+00'), (16, 1, 13, 'note 1', NULL, 1e300, '2025-11-02 05:30:00+00'), (17, 6, 67, 'note_4', NULL, 0.5, '2024-02-29 12:00:00+00'), (18, 1, NULL, 'n%5', NULL, -0, '2025-03-09 06:59:59+00'), (19, 5, -10, 'note 1', -12.125, 1e300, '2024-02-29 12:00:00+00'), (20, 1, -20, 'Note 3', -12.125, -0, '2025-11-02 05:30:00+00'), (21, 1, 91, 'é', 3.333, NULL, '2025-11-02 05:30:00+00'), (22, 1, 89, 'note 2', 0.5, 2.25, '2025-03-09 07:00:00+00'), (23, 1, 158, 'note 2', 3.333, 0.5, '2025-03-09 07:00:00+00'), (24, 1, -39, 'NOTE 3', 0, 'Infinity', '2024-02-29 12:00:00+00'), (25, 1, 19, 'note_4', 1000, 0.5, '2025-03-09 07:00:00+00'), (26, 6, 14, 'é', NULL, 2.25, '2024-02-29 12:00:00+00'), (27, 6, NULL, 'NOTE 3', 3.333, 1e300, '2025-03-09 07:00:00+00'), (28, 1, -33, 'note_4', 0.5, 2.25, NULL), (29, 2, 143, 'é', 0.5, NULL, NULL), (30, 4, 144, 'note_4', -12.125, 1e300, '2025-11-02 05:30:00+00'), (31, 8, 5, 'note 1', 3.333, NULL, '2024-02-29 12:00:00+00'), (32, 9, NULL, 'é', NULL, -0, '2025-11-02 05:30:00+00'), (33, 1, 125, '', 0, NULL, '2024-02-29 12:00:00+00'), (34, 1, 115, 'Note 3', 0.5, 1e300, '2025-03-09 06:59:59+00');

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

CREATE TABLE r7g (id int GENERATED BY DEFAULT AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, 'A', 'A'), (2, 'ss', 'ss'), (3, 'Ä', 'Ä'), (4, '10', '10'), (5, 'Æ', 'Æ'), (6, '9', '9'), (7, 'ß', 'ß'), (8, 'ä b', 'ä b'), (9, 'ä', 'ä'), (10, 'E', 'E'), (11, 'Ä', 'Ä');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 4, 1, 10, '', '2025-01-01'::timestamp + interval '24 hours'), (2, 2, -4, -2.25, NULL, '2025-01-01'::timestamp + interval '33 hours'), (3, 1, -4, 1.50, 'ab', '2025-01-01'::timestamp + interval '171 hours'), (4, 3, NULL, 0, NULL, '2025-01-01'::timestamp + interval '43 hours'), (5, 1, 15, 1.50, 'ab', '2025-01-01'::timestamp + interval '39 hours'), (6, 4, 6, 0, 'B', '2025-01-01'::timestamp + interval '195 hours'), (7, 2, 17, 1.50, '', '2025-01-01'::timestamp + interval '46 hours'), (8, 3, 4, 1.50, 'a', '2025-01-01'::timestamp + interval '49 hours'), (9, 2, 13, 10, 'ab', '2025-01-01'::timestamp + interval '66 hours'), (10, 2, 18, 1.50, 'z', '2025-01-01'::timestamp + interval '75 hours'), (11, 4, 2, NULL, 'b', '2025-01-01'::timestamp + interval '15 hours'), (12, 4, 12, NULL, 'a', '2025-01-01'::timestamp + interval '114 hours'), (13, 4, -1, -2.25, 'a', '2025-01-01'::timestamp + interval '118 hours'), (14, 3, 8, 1.50, 'a', '2025-01-01'::timestamp + interval '102 hours'), (15, 4, 8, 10, '', '2025-01-01'::timestamp + interval '31 hours'), (16, 1, NULL, -2.25, 'a', '2025-01-01'::timestamp + interval '21 hours'), (17, 1, -5, 1.50, 'B', '2025-01-01'::timestamp + interval '2 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 3, 7), (2, 7, 6), (4, 7, 5), (6, 4, 2), (6, 5, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[10,)', '{}', '(2024-12-31,2025-01-05]', '(,0]'), (2, '[8,10)', '{}', NULL, NULL), (3, '[7,8)', '{}', NULL, '[1.5,2.5)'), (4, 'empty', '{[-2,4), [6, 8)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (5, '(4,4]', NULL, '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (6, '[0,)', '{[0,5), [7, 9)}', '(2024-12-31,2025-01-05]', '[0,0]'), (7, '[1,)', '{}', 'empty', NULL), (8, '(6,12]', '{[6,12), [14, 16)}', 'empty', NULL), (9, 'empty', '{(,4)}', NULL, '[1.5,2.5)'), (10, 'empty', '{(,8)}', NULL, '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'brown''s fox''s den'), (2, 'brown''s fox''s den'), (3, 'running rats'), (4, ''), (5, 'foxes and dogs'), (6, 'jumps over the lazy dog'), (7, 'jumps over the lazy dog'), (8, 'dog-eat-dog world'), (9, 'a rat ran');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (5, '[1, "two", 3.5, null, true, {"k": [10, 20]}]');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2143) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2552) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2460) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9c76687 (id int, a int, b text) PARTITION BY RANGE (id);

CREATE TABLE r9c76687_1 PARTITION OF r9c76687 FOR VALUES FROM (MINVALUE) TO (500) WITH (autovacuum_enabled = off);

CREATE TABLE r9c76687_2 PARTITION OF r9c76687 FOR VALUES FROM (500) TO (1500) WITH (autovacuum_enabled = off);

CREATE TABLE r9c76687_3 PARTITION OF r9c76687 DEFAULT WITH (autovacuum_enabled = off);

CREATE UNIQUE INDEX r9c76687_id ON r9c76687 (id);

BEGIN;

COPY r9c76687 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
\.

\s2& INSERT INTO r9c76687 (id, a, b) VALUES (2, 0, 'w2') RETURNING id;

ROLLBACK;

\s2.;

DROP TABLE r9c76687;

CREATE FUNCTION r8f79363(a int, b int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text := ''; i int; j int;
BEGIN
  <<outer>> FOR i IN REVERSE a..b BY 2 LOOP
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

SELECT r8f79363(9, 1);

SELECT r8f79363(8, 2);

SELECT r8f79363(NULL, 1);

DROP FUNCTION r8f79363;

CREATE TABLE r9a99162 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a99162 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2397, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2397) i;

ANALYZE (SKIP_LOCKED) r9a99162;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a99162 WHERE u = 5$$);

DROP TABLE r9a99162;

CREATE TABLE r9a52922 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a52922 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4570, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4570) i;

ANALYZE r9a52922 (g, s, arr);

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a52922 WHERE arr @> '{2}'$$);
