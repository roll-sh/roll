-- class: error_detail
-- ordered: true
-- note: 42725
-- engine: ERROR 42725: function array_agg(unknown) is not unique
-- engine: POSITION: 202
-- reference: ERROR 42725: function array_agg(unknown) is not unique
-- reference: HINT: Could not choose a best candidate function. You might need to add explicit type casts.
-- reference: POSITION: 202

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a text, b int4, c numeric(10,2)) PARTITION BY LIST (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES IN ('a', 'b', 'A');

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b, c) VALUES (18, 'é', -2, 8.50), (2, NULL, -1, 6.00), (21, NULL, 7, 12.25), (36, 'b', 6, 11.75), (40, 'B', 0, -3.75), (23, 'a,b', 1, 2.00), (24, 'a,b', 6, -1.00), (11, 'Z', -2, 6.00), (31, 'A', 6, 7.75), (25, 'B', 6, 9.25), (14, 'a', 1, -2.00), (37, 'Ab', 4, 10.00);

CREATE TABLE t1 (id int4 PRIMARY KEY, a text[], b int8, c float8, d text, e timestamp);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (8, '{z}', 0, NULL, 'Ä', NULL), (17, '{z}', NULL, NULL, NULL, NULL), (5, NULL, NULL, NULL, 'x ', '2024-01-05 06:30:00'), (39, '{a,b}', 3, -1, NULL, '2024-01-08 00:30:00'), (20, '{z}', 5, -2, 'a,b', '2024-01-09 23:30:00'), (40, '{}', 11, 0.25, NULL, NULL), (15, '{A,a}', NULL, 0.75, 'a_b', '2024-01-04 06:00:00'), (34, NULL, NULL, 0, 'foo', NULL), (10, '{ab}', 19, 3.75, '9', '2024-01-09 23:00:00'), (2, '{ab}', NULL, NULL, 'ab', '2024-01-02 23:00:00'), (11, '{a,b}', 9, -1.5, 'a,b', '2024-01-04 23:00:00'), (30, '{ab}', NULL, 0.25, 'a%', NULL), (36, '{A,a}', NULL, NULL, 'A', '2024-01-03 06:00:00'), (9, '{a,b}', 8, NULL, 'Ab', '2024-01-05 00:30:00'), (13, '{z}', NULL, 0.25, 'ß', '2024-01-05 06:30:00'), (21, NULL, 2, 1.25, NULL, '2024-01-07 23:00:00'), (35, NULL, NULL, NULL, 'é', '2024-01-03 00:30:00'), (32, '{}', NULL, -0.75, NULL, '2024-01-03 12:30:00'), (6, '{}', 6, -1.25, NULL, NULL), (28, '{}', 15, NULL, '9', '2024-01-08 06:30:00');

CREATE INDEX t1_i845 ON t1 (c, b);

CREATE TABLE t2 (id int4 PRIMARY KEY, a int8 DEFAULT 20, b timestamptz, c timestamptz, d int8, e timestamp);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (34, NULL, NULL, NULL, NULL, NULL), (11, 13, NULL, NULL, NULL, NULL), (21, NULL, NULL, NULL, NULL, NULL), (1, NULL, NULL, NULL, NULL, NULL), (30, 5, NULL, NULL, NULL, NULL), (4, 15, NULL, NULL, NULL, NULL), (15, NULL, NULL, NULL, -2, NULL), (37, 1, NULL, '2024-01-05 23:00:00+00', NULL, NULL), (36, NULL, '2024-01-04 12:00:00+00', NULL, NULL, '2024-01-06 00:00:00'), (31, NULL, NULL, NULL, 3, NULL), (9, NULL, NULL, NULL, 17, NULL), (39, NULL, NULL, NULL, NULL, NULL), (38, -2, '2024-01-01 06:00:00+00', NULL, 3, NULL), (23, NULL, '2024-01-08 06:00:00+00', '2024-01-01 23:00:00+00', 14, '2024-01-02 00:00:00'), (7, NULL, NULL, NULL, NULL, NULL), (56, -2, NULL, NULL, NULL, NULL), (25, NULL, NULL, NULL, NULL, '2024-01-03 00:00:00'), (12, NULL, '2024-01-04 06:00:00+00', NULL, NULL, '2024-01-04 00:30:00'), (2, NULL, NULL, NULL, NULL, NULL), (47, NULL, NULL, '2024-01-09 12:00:00+00', NULL, NULL);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := (10::int8)::int4;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(position(regexp_replace(NULL::text, '[ab]', 'y', 'g') in (SELECT CASE WHEN false THEN q ELSE t1.d END FROM t1 t1 ORDER BY 1 LIMIT 1)), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN NULL::bool THEN RETURN CASE WHEN p IS NOT DISTINCT FROM 9 THEN ('{3,1,2}'::int4[])[1] ELSE (10.00::numeric)::int4 END;
  ELSE RETURN NULLIF(abs(0), 11);
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE sql STABLE AS $$ SELECT id, d FROM t1 WHERE id <= lim $$;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', '0001-01-01 00:00:00 BC', '-infinity', '1 mon -30 days', '0.5', '1024', 0), (2, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-10-26 00:30:00+00', 'infinity', '2025-01-01', NULL, NULL, NULL, 1), (3, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '-infinity', '2000-01-01 00:00:00', '2025-03-09', '-1 mon 2 days', '0.000001', NULL, 12), (4, '"str"', '"str"', '2025-10-26 01:30:00+00', '2000-01-01 00:00:00', '2025-01-01', '-infinity', '0.5', 'Infinity', -1), (5, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', NULL, '2024-02-29', '36 hours', 'NaN', NULL, 9), (6, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', NULL, '0001-01-01 00:00:00 BC', '2000-03-01', '1 year 2 mons', '0.000001', '-3', 1), (7, NULL, NULL, '1999-12-31 23:59:59.999999+00', '1970-01-01 00:00:00.5', NULL, 'infinity', '1e30', '-0', 3);

INSERT INTO tp4 VALUES (25, '2025-01-01', 'd', '0.000001'), (20, '2025-01-01', 'c', '-0.0'), (9, '2024-02-29', 'a', '-1e-20'), (25, '-infinity', 'c', 'NaN'), (6, '2025-03-09', 'd', 'NaN'), (12, '2025-03-09', 'c', '-Infinity'), (-4, '2024-02-29', NULL, '-0.0'), (14, 'infinity', 'ab', 'NaN'), (-4, NULL, 'b', '0.5'), (12, NULL, 'd', '-Infinity'), (2, '2024-02-29', 'a', '2.5'), (5, '2000-03-01', 'b', '99999999999999999999'), (6, '2024-02-29', 'ab', '-0.0'), (-3, '2025-01-01', 'ab', '-2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "C", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, NULL, 'B', '-99999999999999999999999999999999999.999999', '1 mon', '[5,6)', '{[0,100)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(0, 'SS')::r5ct]), (2, 2, 'ae', 'Infinity', NULL, '[1,5)', '{}', 'the and of', NULL), (3, -3, ' a', NULL, '1 mon', '[5,6)', '{}', NULL, NULL), (4, 5, 'B', '-1e40', '2147483647 days', '[-3,0]', '{}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(0, 'SS')::r5ct, ROW(-1, 'å')::r5ct, ROW(5, '9')::r5ct]), (5, 11, 'I', '7', 'infinity', '(1,5]', '{}', 'running runners ran; the runner runs', NULL), (6, NULL, 'ö', 'NaN', '1 year -1 sec', '(,)', '{empty}', 'the and of', ARRAY[ROW(2, 'a')::r5ct, ROW(1, '2')::r5ct, ROW(2, 'ö')::r5ct]), (7, 8, 'ch', '0', NULL, '[3,)', '{empty}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(1, 'ae')::r5ct, ROW(NULL, 'Ab')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 19 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (16, 'a', NULL), (6, 'b', '2024-02-29'), (-1, 'c', '2024-02-29'), (15, 'd', '2024-02-29') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (23, 'x0', NULL), (28, 'x1', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 0, '{}', '{"plan": "pro", "seats": 3}'), (2, 'u2@x.org', 'bob2', 2, '{}', NULL), (3, 'u3@x.org', 'ann3', 3, '{}', '{"seats": 1.50}'), (4, 'u4@x.org', 'Cid4', 4, NULL, '[1, {"a": null}]'), (5, 'u5@x.org', 'ann5', 5, NULL, NULL), (6, 'u6@x.org', 'Éva6', 6, NULL, '{}'), (7, 'u7@x.org', 'Éva7', 7, '{a}', '{"seats": 1.50}'), (8, 'u8@x.org', 'dee8', 8, NULL, NULL), (9, 'u9@x.org', '9', 0, '{a}', '{}'), (10, 'u10@x.org', 'zoë10', 10, '{a}', '[1, {"a": null}]'), (11, 'u11@x.org', 'zoë11', 0, '{a}', NULL), (12, 'u12@x.org', 'dee12', 12, '{a,b}', '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (9, 0, 'paid', '2025-02-01', NULL), (3, -1.50, 'void', '2025-02-02', '{"items": [{"sku": "a", "qty": 2}]}'), (11, NULL, 'new', '2025-02-23', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 0, 'new', '2025-02-26', '{"gift": true}'), (2, 0, 'paid', '2025-02-16', NULL), (6, 12345678.90, 'paid', '2025-02-25', '{"items": []}'), (4, 0, 'paid', '2025-02-12', NULL), (5, -1.50, 'void', '2025-02-01', '{"gift": true}'), (9, 9.99, 'new', '2025-02-20', '{"items": []}'), (2, 9.99, 'new', '2025-02-10', '{"items": []}'), (1, NULL, 'new', '2025-02-28', '{"items": [{"sku": "a", "qty": 2}]}'), (8, -1.50, 'new', '2025-02-13', NULL), (4, 9.99, 'new', '2025-02-24', '{"gift": true}'), (10, 9.99, 'new', '2025-02-25', '{"items": [{"sku": "a", "qty": 2}]}'), (5, -1.50, 'paid', '2025-02-08', '{"gift": true}'), (1, 9.99, 'paid', '2025-02-11', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 8, 7, '', NULL, -0, '2024-02-29 12:00:00+00'), (2, 1, 55, 'n%5', -12.125, 0.5, '2025-03-09 07:00:00+00'), (3, 2, 112, '', 3.333, NULL, '2024-02-29 12:00:00+00'), (4, 1, 150, 'n%5', -12.125, 2.25, '2025-11-02 05:30:00+00'), (5, 1, 51, 'note 3', 0, 0.5, NULL), (6, 3, 14, 'Note 3', -12.125, 2.25, '2024-02-29 12:00:00+00'), (7, 9, 51, 'note 1', 0, 2.25, '2025-03-09 06:59:59+00'), (8, 1, NULL, 'é', -12.125, 'Infinity', '2025-03-09 07:00:00+00'), (9, 1, 129, '', 1000, 2.25, '2025-03-09 07:00:00+00'), (10, 3, 30, 'NOTE 3', 3.333, 2.25, '2025-11-02 05:30:00+00'), (11, 7, 134, 'NOTE 3', -12.125, -0, '2024-02-29 12:00:00+00'), (12, 1, 22, 'n%5', 3.333, 1e300, '2025-11-02 05:30:00+00'), (13, 1, 102, 'é', 3.333, 2.25, '2025-03-09 06:59:59+00');

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

INSERT INTO r7t VALUES (1, '10', '10'), (2, 'ß', 'ß'), (3, 'A', 'A'), (4, 'Æ', 'Æ'), (5, 'A', 'A'), (6, 'z', 'z'), (7, 'ss', 'ss'), (8, 'ä', 'ä'), (9, 'ä b', 'ä b'), (10, 'Æ', 'Æ'), (11, '9', '9'), (12, 'ae', 'ae'), (13, 'e', 'e'), (14, '9', '9');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 2, 5, 1.50, NULL, '2025-01-01'::timestamp + interval '134 hours'), (2, 1, 1, 1.50, 'ab', '2025-01-01'::timestamp + interval '9 hours'), (3, 1, 7, 10, 'z', '2025-01-01'::timestamp + interval '30 hours'), (4, 1, 5, NULL, 'b', '2025-01-01'::timestamp + interval '58 hours'), (5, 4, NULL, 0, 'a', '2025-01-01'::timestamp + interval '32 hours'), (6, 3, NULL, 10, NULL, '2025-01-01'::timestamp + interval '15 hours'), (7, 2, -5, NULL, 'a', '2025-01-01'::timestamp + interval '33 hours'), (8, 4, NULL, 3.75, 'b', '2025-01-01'::timestamp + interval '177 hours'), (9, 4, 3, 1.50, 'b', '2025-01-01'::timestamp + interval '54 hours'), (10, 1, 10, 10, NULL, '2025-01-01'::timestamp + interval '0 hours'), (11, 2, 8, 1.50, 'B', '2025-01-01'::timestamp + interval '130 hours'), (12, 1, 8, NULL, 'z', '2025-01-01'::timestamp + interval '90 hours'), (13, 4, NULL, 1.50, '', '2025-01-01'::timestamp + interval '100 hours'), (14, 2, 15, NULL, 'a', '2025-01-01'::timestamp + interval '18 hours'), (15, 3, 10, 10, 'b', '2025-01-01'::timestamp + interval '26 hours'), (16, 2, 2, 1.50, 'b', '2025-01-01'::timestamp + interval '96 hours'), (17, 1, NULL, -2.25, 'z', '2025-01-01'::timestamp + interval '147 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 3), (1, 7, 9), (3, 6, 9), (6, 6, 3), (6, 5, 5), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{[5,5), [7, 9)}', '[2025-03-01,infinity)', '[0,0]'), (2, '[0,)', '{[0,1), [3, 5)}', 'empty', '[1.5,2.5)'), (3, '[8,10)', '{(,8)}', '[2025-01-01,2025-02-01)', '[0,0]'), (4, '[-2,)', '{}', '(2024-12-31,2025-01-05]', '(,0]'), (5, '(6,10]', '{[6,10), [12, 14)}', NULL, '[1.5,2.5)'), (6, '[7,)', NULL, NULL, '(1.25,1.5)'), (7, '[-3,)', '{(,-3)}', 'empty', NULL), (8, '(-3,3]', '{}', '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (9, '(-2,-1]', '{}', '[2025-01-01,2025-02-01)', '[0,0]'), (10, NULL, '{(,9)}', '[2025-01-01,2025-02-01)', '[1.5,2.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'foxes and dogs'), (2, 'jumps over the lazy dog'), (3, ''), (4, 'dog-eat-dog world'), (5, 'dog-eat-dog world'), (6, 'brown''s fox''s den'), (7, 'a rat ran');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (6, '[1, "two", 3.5, null, true, {"k": [10, 20]}]');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2347) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2272) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3513) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

CREATE TABLE r9a54254 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a54254 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3643, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3643) i;

ALTER TABLE r9a54254 ALTER COLUMN f SET STATISTICS 16;

SET default_statistics_target = 25;

ALTER TABLE r9a54254 ALTER COLUMN s SET (n_distinct = 0);

UPDATE r9a54254 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a54254 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a54254 WHERE g IN (1, 2, 3) AND b$$);

DROP TABLE r9a54254;

CREATE TABLE r8t58435 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t58435 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf58435() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t58435', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf58435_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t58435', r);
  RETURN NULL;
END $$;

CREATE TRIGGER a_bir BEFORE INSERT OR UPDATE ON r8t58435 FOR EACH ROW EXECUTE FUNCTION r8tf58435('fail');

CREATE TRIGGER d_auof AFTER UPDATE OF v ON r8t58435 FOR EACH ROW WHEN (OLD.v IS DISTINCT FROM NEW.v) EXECUTE FUNCTION r8tf58435('of');

INSERT INTO r8t58435 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t58435.v RETURNING *;

DELETE FROM r8log WHERE tg = 't58435';

INSERT INTO r8t58435 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't58435';

DROP TABLE r8t58435;

DROP FUNCTION r8tf58435, r8tf58435_tt;

CREATE TABLE r8t84314 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t84314 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf84314() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t84314', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf84314_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t84314', r);
  RETURN NULL;
END $$;

CREATE TRIGGER i_td AFTER DELETE ON r8t84314 REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r8tf84314_tt();

CREATE TRIGGER d_auof AFTER UPDATE OF v ON r8t84314 FOR EACH ROW WHEN (OLD.v IS DISTINCT FROM NEW.v) EXECUTE FUNCTION r8tf84314('of');

CREATE TRIGGER b_air AFTER INSERT OR DELETE ON r8t84314 FOR EACH ROW EXECUTE FUNCTION r8tf84314();

CREATE TRIGGER f_tt AFTER UPDATE ON r8t84314 REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r8tf84314_tt();

CREATE TRIGGER f_tt AFTER UPDATE ON r8t84314 REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r8tf84314_tt();

INSERT INTO r8t84314 VALUES (10, -1, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't84314';

UPDATE r8t84314 SET v = NULL WHERE id = 1 RETURNING id, v, s;

DELETE FROM r8log WHERE tg = 't84314';

INSERT INTO r8t84314 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t84314.v RETURNING *;

DELETE FROM r8log WHERE tg = 't84314';

DELETE FROM r8t84314 WHERE id = 3 RETURNING *;

DELETE FROM r8log WHERE tg = 't84314';

UPDATE r8t84314 SET v = coalesce(v, 0) - 1 RETURNING id, v;

DELETE FROM r8log WHERE tg = 't84314';

INSERT INTO r8t84314 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't84314';

BEGIN;

UPDATE r8t84314 SET s = 'deferred' WHERE id = 2;

COMMIT;

COMMIT;

DELETE FROM r8log WHERE tg = 't84314';

DROP TABLE r8t84314;

DROP FUNCTION r8tf84314, r8tf84314_tt;

-- statement
SELECT s1.c0 AS c0 FROM ((SELECT DISTINCT ((to_jsonb(true) ->> 'a') || ((s2.c1 - '1999-07-18'::date))::text) AS c0, concat_ws('-', ((0.125::float8)::text COLLATE "en-x-icu"), '10') AS c1 FROM ((SELECT array_agg('foo' ORDER BY 'foo') FILTER (WHERE t3.id BETWEEN t3.id AND t3.id) AS c0, max('2024-04-08'::date) AS c1, count(*) AS c2, min(num_nonnulls((7::int8)::int4, lower(t3.d))) AS c3 FROM t1 t3) s2 RIGHT JOIN t2 t4 ON s2.c2 = t4.d) WHERE (range_merge(daterange(LEAST(make_date(2024, 11, 1), s2.c1), GREATEST(make_date(2024, 11, 1), s2.c1)), 'empty'::daterange))::text NOT LIKE 'a_b') s1 JOIN t2 t5 ON t5.id < ('1999-01-07'::date - '2025-06-15'::date)) WHERE false ORDER BY 1 NULLS FIRST OFFSET 0;
