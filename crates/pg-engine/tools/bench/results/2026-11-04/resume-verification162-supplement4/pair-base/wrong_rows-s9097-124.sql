-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a17534  (rows=21)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a17534  (rows=1372)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a jsonb, b interval, c text[], g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b, c) VALUES (17, '[1, 2, 3]', NULL, NULL), (26, NULL, NULL, '{}'), (6, NULL, NULL, '{a,b}'), (16, '{"b": [1, "a"]}', '1 day', '{ab}'), (15, NULL, '1 day', '{b,NULL}'), (33, NULL, '1 mon', '{}'), (19, NULL, '1 day', '{A,a}'), (40, '{"a": 3}', '1 day', NULL);

CREATE INDEX t0_i56 ON t0 USING gin (a);

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamptz, b int4[], c int4, d json, e int4[], g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (24, '2024-01-03 06:00:00+00', NULL, 5, NULL, '{1,2}'), (14, NULL, NULL, NULL, '{"a": 3}', '{5}'), (6, NULL, '{3,1,2}', NULL, NULL, '{5}'), (19, NULL, NULL, NULL, NULL, NULL), (45, NULL, '{3,1,2}', NULL, NULL, NULL), (31, NULL, '{NULL,3}', NULL, NULL, '{2}'), (15, NULL, NULL, NULL, '{"a": {"b": 2}}', NULL), (12, NULL, '{2}', NULL, NULL, '{3,1,2}'), (11, NULL, NULL, NULL, '1', NULL), (27, NULL, NULL, NULL, NULL, NULL), (52, NULL, '{NULL,3}', NULL, NULL, '{}'), (20, NULL, NULL, NULL, NULL, NULL);

CREATE INDEX t1_i426 ON t1 (e);

CREATE INDEX t1_i945 ON t1 (g);

CREATE INDEX t1_i163 ON t1 (b) WHERE b IS NOT NULL;

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 THEN RETURN NULL; END IF; NEW.c := coalesce(NEW.c, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER DELETE ON t1 REFERENCING OLD TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW(NULL, 0)::ct3, '{}', 3), (2, ROW('9', NULL)::ct3, ARRAY[ROW('é', -1)::ct3, ROW(NULL, 1)::ct3], 3), (3, ROW('ba', -1)::ct3, ARRAY[ROW('Ab', 0)::ct3, ROW('B', -1)::ct3], 1), (4, ROW('', 0)::ct3, ARRAY[ROW('Z', 2)::ct3, ROW('a,b', 2)::ct3], 3), (5, ROW('ß', 2)::ct3, '{}', NULL);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE AS $$
DECLARE r int := 0;
BEGIN
  IF EXISTS (SELECT p FROM t0 t2) THEN
    r := (abs(10::int8))::int4;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(jsonb_array_length('"s"'::jsonb), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := GREATEST(p, (p % 4));
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(('{"a": {"b": 2}}'::jsonb::jsonb ->> 'a')::int, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
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

INSERT INTO tj4 VALUES (1, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2024-02-29 23:00:00+00', '2025-03-09 02:30:00', '2000-03-01', '-1 mon 2 days', '-Infinity', '-0', -2), (2, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', 'infinity', 'infinity', '-infinity', NULL, '0', NULL, 9), (3, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-03-09 03:30:00-04', '0001-01-01 00:00:00 BC', 'infinity', 'infinity', '0.000001', NULL, 12), (4, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', NULL, '2025-03-09 02:30:00', '2024-02-29', NULL, '123.45', '0.5', 12), (5, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', 'infinity', '2024-02-29', 'infinity', '0.000001', 'NaN', NULL), (6, 'null', 'null', '2025-11-02 01:30:00-05', '2025-12-31 23:59:59.999999', '1999-12-31', '1 day', '2.5', 'Infinity', 1), (7, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '2025-12-31 23:59:59.999999', NULL, '1 year 2 mons', '0', '-1.25', 0), (8, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', 'infinity', '2000-03-01', '1 day', NULL, '1024', 1), (9, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2025-10-26 02:30:00', '1999-12-31', 'infinity', NULL, '1024', NULL), (10, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '2025-03-09 02:30:00', '4713-01-01 BC', '0', '0.000001', NULL, 8), (11, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '1999-12-31 23:59:59.999999+00', '2025-11-02 01:30:00', '2025-03-09', 'P1DT12H', 'Infinity', '1024', NULL), (12, NULL, NULL, '2025-04-06 02:30:00+11', 'infinity', '2025-03-09', '1 day', '-Infinity', NULL, 10), (13, NULL, NULL, '0044-03-15 12:00:00+00 BC', '2000-01-01 00:00:00', '2025-01-01', '2 weeks 3 microseconds', NULL, 'NaN', 5), (14, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', 'infinity', '2025-01-01', '0', '1.23456789012345678901234567890', '-1.25', 12);

INSERT INTO tp4 VALUES (23, '2000-03-01', 'b', '0.5'), (14, '2000-03-01', 'c', '0.5'), (9, '4713-01-01 BC', 'c', '0'), (3, NULL, 'ab', 'Infinity'), (25, '-infinity', 'c', NULL), (-5, '4713-01-01 BC', NULL, '0.5'), (16, '2025-01-01', 'b', '-Infinity'), (-3, '2024-02-29', 'b', '-1e-20'), (13, 'infinity', 'ab', '-Infinity'), (-2, '2024-02-29', 'b', '1e30'), (15, '2025-01-01', 'a', '0.000001'), (23, '2000-03-01', 'c', '2.5'), (3, 'infinity', 'a', '-1e-20'), (-3, '2025-03-09', 'c', '-Infinity');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5num", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 5, 'e', '0.0000000000000000000000000001', '1 day 00:00:00.000001', '(1,5]', '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(2, '9')::r5ct, ROW(5, 'ab')::r5ct]), (2, 14, 'c', NULL, '720:00:00', '[0,10)', NULL, NULL, ARRAY[ROW(-2, '')::r5ct, ROW(5, 'a-')::r5ct]), (3, 12, 'a', '0.0000000000000000000000000001', '2147483647 days', '(,)', '{[1,3),[5,7)}', 'the and of', ARRAY[ROW(-2, '')::r5ct, ROW(3, 'a-')::r5ct, ROW(2, 'o')::r5ct]), (4, 10, NULL, '2.5', '-178000000 years', '(,2]', '{empty}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(NULL, 'I')::r5ct]), (5, 3, 'ö', NULL, '-infinity', NULL, '{(,0],[10,)}', 'running runners ran; the runner runs', '{}'), (6, 8, 'ä', '2.5', '2147483647 days', '(,)', '{[0,100)}', 'the and of', ARRAY[ROW(3, '2')::r5ct, ROW(NULL, 'a-')::r5ct, ROW(0, NULL)::r5ct]), (7, 11, 'a-', 'NaN', '-2147483648 days', '(,)', '{[2,4)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(NULL, 'ö')::r5ct, ROW(-2, 'İ')::r5ct]), (8, 17, NULL, NULL, NULL, '[0,10)', '{[0,100)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(5, NULL)::r5ct]), (9, 2, 'i', '1e100', '1 year -1 sec', '(,2]', '{empty}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(-2, 'ä')::r5ct]);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 10 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (15, 'a', '2025-01-01'), (19, 'b', '2024-02-29'), (8, 'c', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (24, 'x0', NULL), (32, 'x1', NULL), (22, 'x2', NULL), (24, 'x3', NULL), (28, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'zoë1', 1, '{}', NULL), (2, 'u2@x.org', '2', 2, '{a}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'zoë3', 3, '{}', '[1, {"a": null}]'), (4, 'u4@x.org', 'ann4', 4, '{b,NULL}', '{}'), (5, 'u5@x.org', 'bob5', 0, '{}', '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 0, 'paid', '2025-02-09', '{"items": []}'), (4, -1.50, 'new', '2025-02-03', '{"items": []}'), (4, 100, 'paid', '2025-02-02', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 100, 'paid', '2025-02-04', NULL);

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

INSERT INTO r7k VALUES (1, 1, -33, 'n%5', NULL, NULL, '2025-03-09 07:00:00+00'), (2, 2, 38, 'note 1', 0.5, 1e300, NULL), (3, 1, 22, 'note_4', NULL, 2.25, '2025-03-09 06:59:59+00'), (4, 4, 88, 'note 2', -12.125, 0.5, '2024-02-29 12:00:00+00'), (5, 1, 48, '', 1000, -0, '2025-03-09 07:00:00+00'), (6, 1, 97, 'note 1', 0, 0.5, '2024-02-29 12:00:00+00'), (7, 1, 30, 'note_4', 1000, NULL, '2025-03-09 07:00:00+00'), (8, 1, 127, 'note_4', 0, 'Infinity', '2025-03-09 07:00:00+00'), (9, 1, 0, 'NOTE 3', -12.125, 'Infinity', '2024-02-29 12:00:00+00'), (10, 7, -36, 'note_4', 0.5, NULL, '2025-03-09 07:00:00+00'), (11, 1, 97, 'é', NULL, 'Infinity', '2025-03-09 07:00:00+00'), (12, 1, 143, 'note 2', 0.5, 0.5, '2025-11-02 05:30:00+00'), (13, 1, -27, 'Note 3', NULL, 2.25, '2024-02-29 12:00:00+00'), (14, 4, 112, 'note 3', 0, -0, '2025-03-09 06:59:59+00');

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

INSERT INTO r7t VALUES (1, 'Ä', 'Ä'), (2, 'e', 'e'), (3, 'ß', 'ß'), (4, 'e', 'e'), (5, 'E', 'E'), (6, '9', '9'), (7, 'a', 'a'), (8, 'ae', 'ae'), (9, 'A', 'A'), (10, 'b', 'b');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 1, NULL, 0, 'a', '2025-01-01'::timestamp + interval '57 hours'), (2, 2, 7, 3.75, 'B', '2025-01-01'::timestamp + interval '99 hours'), (3, 1, 12, 10, 'b', '2025-01-01'::timestamp + interval '28 hours'), (4, 3, 7, 3.75, 'z', '2025-01-01'::timestamp + interval '55 hours'), (5, 2, NULL, 1.50, 'B', '2025-01-01'::timestamp + interval '60 hours'), (6, 1, 12, 3.75, 'ab', '2025-01-01'::timestamp + interval '18 hours'), (7, 4, 8, 1.50, 'ab', '2025-01-01'::timestamp + interval '27 hours'), (8, 3, NULL, -2.25, 'b', '2025-01-01'::timestamp + interval '40 hours'), (9, 4, 14, 1.50, '', '2025-01-01'::timestamp + interval '88 hours'), (10, 4, 7, 3.75, 'a', '2025-01-01'::timestamp + interval '138 hours'), (11, 1, -3, 1.50, 'B', '2025-01-01'::timestamp + interval '26 hours'), (12, 1, 1, -2.25, 'b', '2025-01-01'::timestamp + interval '33 hours'), (13, 3, -5, 10, 'ab', '2025-01-01'::timestamp + interval '4 hours'), (14, 4, -1, 0, 'b', '2025-01-01'::timestamp + interval '62 hours'), (15, 2, -4, 0, NULL, '2025-01-01'::timestamp + interval '61 hours'), (16, 2, 4, 1.50, NULL, '2025-01-01'::timestamp + interval '132 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 1, 7), (1, 3, 8), (2, 4, 3), (2, 2, 9), (4, 2, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{[10,12), [14, 16)}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (2, NULL, '{[5,6), [8, 10)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (3, 'empty', '{[7,10), [12, 14)}', '[2025-01-01,2025-02-01)', '(1.25,1.5)'), (4, NULL, '{[-2,-2), [0, 2)}', '[2025-03-01,infinity)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, 'quick quick slow'), (3, 'dog-eat-dog world'), (4, 'brown''s fox''s den'), (5, 'dog-eat-dog world'), (6, 'jumps over the lazy dog'), (7, 'brown''s fox''s den');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (5, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3217) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2320) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2705) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a87731 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a87731 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2371, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2371) i;

ALTER TABLE r9a87731 ALTER COLUMN s SET (n_distinct = 50);

CREATE STATISTICS r9a87731_st (ndistinct) ON (g + 1), b FROM r9a87731;

UPDATE r9a87731 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE (SKIP_LOCKED) r9a87731;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a87731 WHERE s LIKE 'v1%'$$);

SELECT * FROM r9est($$SELECT * FROM r9a87731 WHERE g = 3$$);

SELECT * FROM r9est($$SELECT * FROM r9a87731 WHERE s = 'v7'$$);

DROP TABLE r9a87731;

\s2 BEGIN;

\s2 UPDATE r8a SET v = coalesce(v, 0) + 100 WHERE id = 3 RETURNING id, v;

\s2 INSERT INTO r8log (tg, info) VALUES ('2pc', 'r8gx88956');

\s2 PREPARE TRANSACTION 'r8gx88956';

SELECT gid, owner, database = current_database() AS here FROM pg_prepared_xacts ORDER BY gid;

\s2 PREPARE TRANSACTION 'r8gx88956';

\s3& UPDATE r8a SET v = -1 WHERE id = 3 RETURNING id, v;

SELECT 1;

ROLLBACK;

\s2 ROLLBACK;

ROLLBACK PREPARED 'r8gx88956';

\s3.;

COMMIT PREPARED 'r8gx88956';

SELECT count(*) FROM pg_prepared_xacts WHERE gid = 'r8gx88956';

\x3 name=r9x54151 describe p=t'1' p=t'49'
SELECT id, v FROM r9s WHERE g = $1 AND v > $2 ORDER BY v, id LIMIT 5;

\x3 name=r9x54151 p=t'8' p=t'-50'
-;

\x3 name=r9x54151 p=b00000008 p=t'3000000000'
-;

\x3 name=r9x54151 rf=1,0 p=t'8' p=t'-50'
-;

\x3 name=r9x54151 p=b00000001 p=t'49'
-;

\x3 name=r9x54151 rf=0 p=t'2' p=t'0'
-;

\s3 SET plan_cache_mode = auto;

\x3 name=r9x54151 p=b00000001 p=bffffffff
-;

\x3 name=r9x54151 p=b00000001 p=b00000007
-;

\x3 name=r9x54151 rf=1 p=b00000003 p=t'0'
-;

\x3 name=r9x54151 rf=1 p=t'8' p=t'3000000000'
-;

\x3 name=r9x54151 rf=1 p=t'2' p=t'49'
-;

\s3 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x54151';

\s3 DEALLOCATE r9x54151;

\s3 RESET plan_cache_mode;

EXPLAIN (COSTS OFF) SELECT id, t FROM r9s WHERE t >= 'a b' AND t < 'b' ORDER BY t , id LIMIT 20;

SET enable_indexscan = off;

RESET enable_indexscan;

CREATE TABLE r9a13141 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a13141 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3241, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3241) i;

CREATE STATISTICS r9a13141_st (ndistinct, dependencies) ON g, s FROM r9a13141;

UPDATE r9a13141 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE (SKIP_LOCKED) r9a13141;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a13141 WHERE g = 3$$);

SELECT * FROM r9est($$SELECT * FROM r9a13141 WHERE NOT b$$);

DROP TABLE r9a13141;

CREATE TABLE r8t69473 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t69473 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf69473() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t69473', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf69473_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t69473', r);
  RETURN NULL;
END $$;

CREATE CONSTRAINT TRIGGER h_def AFTER INSERT OR UPDATE ON r8t69473 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION r8tf69473('deferred');

CREATE TRIGGER e_tr BEFORE TRUNCATE ON r8t69473 FOR EACH STATEMENT EXECUTE FUNCTION r8tf69473();

CREATE CONSTRAINT TRIGGER h_def AFTER INSERT OR UPDATE ON r8t69473 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION r8tf69473('deferred');

CREATE TRIGGER k_bad BEFORE UPDATE ON r8t69473 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r8tf69473_tt();

INSERT INTO r8t69473 VALUES (10, 5, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't69473';

UPDATE r8t69473 SET v = 0 WHERE id = 1 RETURNING id, v, s;

DELETE FROM r8log WHERE tg = 't69473';

UPDATE r8t69473 SET v = coalesce(v, 0) - 1 RETURNING id, v;

DELETE FROM r8log WHERE tg = 't69473';

INSERT INTO r8t69473 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't69473';

DROP TABLE r8t69473;

DROP FUNCTION r8tf69473, r8tf69473_tt;

SET TimeZone = 'UTC';

SET extra_float_digits = 1;

SELECT lo_from_bytea(979355, '\x00ff00ff')::int8 - 979355;

SELECT lo_get(979355), lo_get(979355, 0, 0);

BEGIN;

SELECT lo_open(979355, x'60000'::int);

SELECT loread(0, 3);

SELECT lowrite(0, '\x5a5a');

SELECT loread(0, 100);

SELECT lo_close(0);

COMMIT;

SELECT lo_get(979355);

SELECT oid::int8 - 979355, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = 979355;

GRANT SELECT ON LARGE OBJECT 979355 TO r7_bob;

SELECT lo_unlink(979355);

SELECT lo_get(979355);

CREATE TABLE r9a17534 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a17534 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4116, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4116) i;

CREATE STATISTICS r9a17534_st (ndistinct, dependencies) ON g, s FROM r9a17534;

ANALYZE r9a17534 (g, s, arr);

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a17534 WHERE arr @> '{2}'$$);
