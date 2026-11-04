-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: QUERY PLAN:25
-- engine: Index Scan using r9s_pkey on r9s
-- engine:   Index Cond: (id = ANY ('{1,2,3}'::integer[]))
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Index Scan using r9s_pkey on r9s
-- reference:   Index Cond: (id = ANY (('{1,2,3}'::cstring)::integer[]))
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4[], b float8, c timestamp, d numeric(10,2), e numeric(10,2), g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (39, '{NULL,3}', 0, '2024-01-04 00:00:00', -1.05, 0.05), (30, '{3,1,2}', 3.25, '2024-01-08 00:30:00', 0.00, 11.50), (16, '{NULL,3}', -1, '2024-01-05 12:00:00', -1.00, 0.00), (4, '{3,1,2}', 3.75, '2024-01-07 00:00:00', 5.25, 6.25), (11, '{}', -1.75, '2024-01-07 12:00:00', NULL, 6.50), (9, '{5}', 3.75, NULL, 9.05, -3.00), (6, '{NULL,3}', 3.5, NULL, -2.00, 12.05), (51, '{1,2}', -0.25, '2024-01-02 23:00:00', 5.25, -1.05), (28, NULL, NULL, '2024-01-07 00:00:00', 2.00, 1.75), (37, '{NULL,3}', 3.5, NULL, 9.00, 4.25), (26, '{}', 0.75, '2024-01-01 12:00:00', 6.00, -3.00), (23, '{1,2}', 3.25, '2024-01-08 23:30:00', 2.50, -3.00), (34, NULL, 3.75, '2024-01-01 12:00:00', 3.05, 2.75), (24, '{3,1,2}', 2, '2024-01-03 06:30:00', 4.00, 9.00), (12, '{NULL,3}', -1.25, '2024-01-02 00:00:00', 5.00, 8.00), (21, '{2}', 3, '2024-01-02 23:30:00', 4.50, -2.00), (33, NULL, 3, '2024-01-02 23:00:00', NULL, 5.00), (49, '{}', -1.25, '2024-01-05 06:30:00', 6.00, 10.05), (3, '{1,2}', 3.25, '2024-01-03 00:00:00', -2.25, 9.05), (1, '{NULL,3}', 2.75, '2024-01-07 06:30:00', 4.00, 4.05);

CREATE INDEX t0_i395 ON t0 (a, id);

CREATE INDEX t0_i639 ON t0 (e) WHERE e IS NOT NULL;

CREATE INDEX t0_i287 ON t0 (id);

CREATE TABLE t1 (id int4 PRIMARY KEY, a numeric(10,2) DEFAULT 0, b numeric(10,2), c numeric(10,2));

INSERT INTO t1 (id, a, b, c) VALUES (10, NULL, NULL, NULL), (24, 0.00, NULL, NULL), (16, NULL, NULL, 2.25), (37, 1.75, 11.75, NULL), (23, NULL, NULL, NULL), (34, NULL, 7.25, 0.50), (11, 0.75, NULL, NULL), (28, 5.25, NULL, -3.00), (22, NULL, NULL, NULL), (21, NULL, 10.00, NULL), (14, NULL, NULL, 9.25), (19, 3.05, NULL, NULL);

CREATE INDEX t1_i276 ON t1 (b);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t2 (id int4 PRIMARY KEY, a numeric(10,2), b interval, c text, d jsonb);

INSERT INTO t2 (id, a, b, c, d) VALUES (15, NULL, '2 hours', 'ba', NULL), (11, NULL, '1 mon', 'aa', '{"a": 3}'), (39, 1.75, '24:00:00', 'Ä', '{"a": {"b": 2}}'), (25, 8.75, NULL, NULL, 'null'), (24, 3.05, '1 day', 'a_b', 'null'), (19, 6.05, '2 hours', NULL, '"s"'), (8, 2.50, NULL, 'Ab', '"s"'), (45, NULL, '-1 day', NULL, NULL), (33, 8.75, '24:00:00', 'A', NULL), (9, 5.50, '0', NULL, '{"a": {"b": 2}}'), (5, 1.00, '-1 day', NULL, '[1, 2, 3]'), (38, 2.75, '2 hours', 'a%', NULL);

CREATE INDEX t2_i823 ON t2 (b);

CREATE INDEX t2_i313 ON t2 (a DESC NULLS LAST);

CREATE TYPE ct3 AS (a text COLLATE "C", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, NULL, NULL, 1), (2, ROW(NULL, -1)::ct3, ARRAY[ROW('ß', NULL)::ct3, ROW('abc', 3)::ct3], 2), (3, NULL, ARRAY[ROW('Z', 2)::ct3, ROW('z', 3)::ct3], 4), (4, ROW(' x', 0)::ct3, ARRAY[ROW(NULL, NULL)::ct3, ROW('10', 1)::ct3], 0), (5, NULL, '{}', NULL), (6, ROW('bar', NULL)::ct3, ARRAY[ROW(' x', 1)::ct3, ROW('aa', NULL)::ct3], 3), (7, ROW('abc', 1)::ct3, '{}', 4);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN true THEN 8 ELSE CASE WHEN false THEN abs(p) ELSE ascii('a%') END END $$;

CREATE VIEW v0 AS SELECT t2.e AS c0, (SELECT '{}'::int4[] FROM t0 t3 WHERE ((true) IS FALSE AND false) ORDER BY 1 LIMIT 1) AS c1, max((CASE WHEN true THEN t2.g ELSE 1 END)::int2) AS c2 FROM (t2 t1 JOIN t0 t2 ON t1.a = t2.d) WHERE isfinite('2025-09-19'::date) GROUP BY t2.e, t1.id;

CREATE VIEW v1 AS SELECT ((t1.g << 1))::float8 AS c0, (t1.g)::int2 AS c1, '1 year 2 days'::interval AS c2, 'b' AS c3 FROM t0 t1 WHERE ((to_jsonb('a_b')::jsonb ->> 'a')::int, array_to_string(string_to_array('foo', ','), ',', '?')) = (((SELECT extract(isodow from '2024-01-07 08:00:00-05'::timestamptz)::int FROM t0 t2 ORDER BY 1 LIMIT 1) * NULLIF(11, t1.g)), NULL::text);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2024-02-29 23:00:00+00', '2025-10-26 02:30:00', '2025-01-01', '-infinity', '-0.0', 'NaN', NULL), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', NULL, '2025-03-09 02:30:00', '2000-03-01', NULL, NULL, '-3', 0), (3, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-11-02 01:30:00-05', '2024-02-29 00:00:00', '2000-03-01', '1 mon -30 days', '-1e-20', '-1.25', 8), (4, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2038-01-19 03:14:08+00', '2025-11-02 01:30:00', '2025-01-01', '1 day', '-0.0', NULL, 3), (5, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-03-09 03:30:00-04', '1970-01-01 00:00:00.5', '2024-02-29', '2 weeks 3 microseconds', '1e30', '-1.25', -2), (6, 'null', 'null', NULL, '0001-01-01 00:00:00 BC', '-infinity', NULL, '99999999999999999999', '0.5', 9), (7, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2038-01-19 03:14:08+00', '2025-12-31 23:59:59.999999', '-infinity', NULL, '0.5', '2', 11), (8, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2025-10-26 00:30:00+00', '1970-01-01 00:00:00.5', '2025-01-01', '-12:00', '1e30', '2', -3);

INSERT INTO tp4 VALUES (-1, NULL, 'd', '0.000001'), (3, '-infinity', 'ab', '99999999999999999999'), (-3, 'infinity', 'd', '1.23456789012345678901234567890'), (12, NULL, 'b', 'Infinity'), (-5, '2025-03-09', 'a', '-Infinity'), (19, NULL, 'd', '0.5'), (4, '-infinity', 'c', '123.45'), (9, '2024-02-29', 'b', '-7'), (22, '2000-03-01', 'ab', '2.5'), (14, '2024-02-29', 'a', '-1e-20'), (21, '-infinity', 'ab', '0.5'), (1, '2025-01-01', 'ab', '2.5'), (18, NULL, 'b', '2.5'), (19, '-infinity', 'c', '0.5');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "C", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 10, 'c', '0.0000000000000000000000000001', '-178000000 years', '(1,5]', NULL, 'running runners ran; the runner runs', ARRAY[ROW(NULL, 'ae')::r5ct]), (2, 11, 'ß', '123456789012345678901234567890.123456789', '1 day 00:00:00.000001', '[3,)', '{}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(4, 'é')::r5ct]), (3, NULL, 'Z', '-0.5', '1 day 00:00:00.000001', '(,)', '{}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(NULL, 'İ')::r5ct, ROW(3, '-a')::r5ct]), (4, 1, 'a-', 'NaN', '178000000 years', '(1,5]', '{[0,100)}', NULL, '{}'), (5, 9, 'ß', '-Infinity', 'infinity', '(,)', '{empty}', 'the and of', ARRAY[ROW(2, 'ae')::r5ct, ROW(-1, NULL)::r5ct]), (6, 8, 'Z', 'NaN', '-1 mon 30 days', '[0,10)', '{[1,2],[2,3)}', NULL, '{}'), (7, 10, 'é', '7', '178000000 years', '(1,5]', '{(,0],[10,)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(0, 'b')::r5ct, ROW(-1, 'é')::r5ct]), (8, -1, 'ae', '-1e40', '1 day 00:00:00.000001', NULL, '{(,0],[10,)}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (9, -3, 'o', 'NaN', '2147483647 mons', '(,2]', '{[1,2],[2,3)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-2, 'E')::r5ct]), (10, 16, 'B', '1e100', '178000000 years', '[0,10)', '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', '{}'), (11, 0, 'ae', '123456789012345678901234567890.123456789', '1 day 00:00:00.000001', '(,2]', '{[2,4)}', NULL, ARRAY[ROW(4, '9')::r5ct, ROW(-2, 'AB')::r5ct, ROW(4, 'ß')::r5ct]), (12, 15, '-a', NULL, '30 days', '(,)', '{empty}', 'the and of', '{}');

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_b ON r5a (b COLLATE "r5ci");

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 18;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (16, 'a', NULL), (-4, 'b', NULL) ON CONFLICT DO NOTHING;

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 1, '{a,b}', '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'Éva2', 2, '{}', NULL), (3, 'u3@x.org', 'bob3', 0, NULL, '{"seats": 1.50}'), (4, 'u4@x.org', 'ann4', 4, '{b,NULL}', '[1, {"a": null}]'), (5, 'u5@x.org', 'bob5', 5, '{}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (2, 9.99, 'void', '2025-02-07', NULL), (1, 12345678.90, 'paid', '2025-02-11', '{"gift": true}'), (4, NULL, 'new', '2025-02-06', NULL), (1, -1.50, 'void', '2025-02-05', NULL), (4, 100, 'paid', '2025-02-23', '{"items": []}'), (4, -1.50, 'void', '2025-02-05', '{"gift": true}'), (3, 9.99, 'paid', '2025-02-03', NULL), (4, -1.50, 'paid', '2025-02-07', NULL), (2, 9.99, 'void', '2025-02-18', NULL), (5, 9.99, 'paid', '2025-02-23', NULL), (5, NULL, 'void', '2025-02-09', '{"items": []}'), (4, 12345678.90, 'void', '2025-02-20', NULL), (1, 0, 'void', '2025-02-07', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 12345678.90, 'new', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 12345678.90, 'new', '2025-02-08', '{"items": []}'), (1, NULL, 'void', '2025-02-21', NULL), (3, 12345678.90, 'void', '2025-02-01', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, NULL, 'n%5', 1000, 2.25, '2024-02-29 12:00:00+00'), (2, 1, 158, 'note 2', 0.5, 'Infinity', '2025-03-09 06:59:59+00'), (3, 9, 190, 'note 3', 0, 2.25, NULL), (4, 1, 69, 'NOTE 3', NULL, 1e300, '2025-03-09 06:59:59+00'), (5, 1, 174, 'Note 3', 0.5, 1e300, '2025-03-09 07:00:00+00'), (6, 1, 154, 'Note 3', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (7, 4, 97, 'Note 3', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (8, 8, NULL, '', 3.333, 'NaN', '2024-02-29 12:00:00+00'), (9, 7, NULL, 'note_4', NULL, 0.5, '2024-02-29 12:00:00+00'), (10, 6, 150, 'note_4', NULL, 'NaN', '2025-03-09 06:59:59+00'), (11, 1, 98, 'note 1', 3.333, 2.25, '2025-11-02 05:30:00+00'), (12, 6, 74, 'note_4', 0.5, 2.25, NULL), (13, 5, NULL, '', 0, 'NaN', '2024-02-29 12:00:00+00'), (14, 1, 47, 'é', 1000, 2.25, NULL), (15, 1, 42, 'note 2', -12.125, 'Infinity', '2025-03-09 07:00:00+00'), (16, 1, 185, 'é', 1000, -0, NULL), (17, 1, 66, 'Note 3', 1000, 1e300, '2024-02-29 12:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21);

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

INSERT INTO r7t VALUES (1, 'ss', 'ss'), (2, 'Ä', 'Ä'), (3, 'a', 'a'), (4, 'ae', 'ae'), (5, 'ß', 'ß'), (6, 'ß', 'ß'), (7, 'ae', 'ae'), (8, 'z', 'z'), (9, 'e', 'e'), (10, 'ae', 'ae'), (11, '9', '9'), (12, 'a', 'a'), (13, '9', '9'), (14, 'é', 'é');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

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

INSERT INTO r8a VALUES (1, 2, 4, 3.75, 'b', '2025-01-01'::timestamp + interval '156 hours'), (2, 2, -2, -2.25, 'ab', '2025-01-01'::timestamp + interval '189 hours'), (3, 2, 18, 3.75, 'a', '2025-01-01'::timestamp + interval '72 hours'), (4, 4, -5, 1.50, 'a', '2025-01-01'::timestamp + interval '12 hours'), (5, 1, 12, NULL, '', '2025-01-01'::timestamp + interval '140 hours'), (6, 4, 10, NULL, 'B', '2025-01-01'::timestamp + interval '4 hours'), (7, 3, NULL, -2.25, 'a', '2025-01-01'::timestamp + interval '53 hours'), (8, 4, 7, 10, 'ab', '2025-01-01'::timestamp + interval '78 hours'), (9, 2, NULL, 3.75, 'ab', '2025-01-01'::timestamp + interval '21 hours'), (10, 1, 5, 3.75, 'B', '2025-01-01'::timestamp + interval '2 hours'), (11, 2, 7, 10, 'ab', '2025-01-01'::timestamp + interval '112 hours'), (12, 4, -5, 3.75, NULL, '2025-01-01'::timestamp + interval '134 hours'), (13, 3, 2, 10, '', '2025-01-01'::timestamp + interval '74 hours'), (14, 1, -1, 1.50, '', '2025-01-01'::timestamp + interval '171 hours'), (15, 4, 8, 1.50, 'z', '2025-01-01'::timestamp + interval '39 hours'), (16, 2, -5, NULL, NULL, '2025-01-01'::timestamp + interval '63 hours'), (17, 2, 6, -2.25, 'z', '2025-01-01'::timestamp + interval '42 hours'), (18, 2, -1, 1.50, '', '2025-01-01'::timestamp + interval '56 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 3, 7), (1, 6, 5), (2, 1, 4), (2, 5, 6), (4, 4, 6), (6, 3, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-1,1)', NULL, '[2025-03-01,infinity)', NULL), (2, '[10,15)', '{(,10)}', NULL, NULL), (3, 'empty', '{(,3)}', '[2025-01-01,2025-02-01)', '[0,0]'), (4, NULL, NULL, 'empty', NULL), (5, '[-3,1)', '{[-3,1), [3, 5)}', 'empty', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'foxes and dogs'), (2, 'brown''s fox''s den'), (3, 'the quick brown fox'), (4, 'jumps over the lazy dog');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2936) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2658) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2360) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

SELECT sum(v) OVER (ORDER BY g ROWS UNBOUNDED FOLLOWING) FROM r8a;

SELECT jsonb_path_query('{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}', 'strict $.*.a', '{"x": 1}', false);

CREATE FUNCTION r8f49733(a int, b int) RETURNS text LANGUAGE plpgsql AS $$
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

SELECT r8f49733(9, 1);

SELECT r8f49733(4, -2);

SELECT r8f49733(NULL, 1);

DROP FUNCTION r8f49733;

SELECT JSON_EXISTS(jsonb '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}', '$.a.integer()' PASSING 1 AS x ERROR ON ERROR), JSON_EXISTS(jsonb '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}', 'strict $.a.integer()' ERROR ON ERROR);

SET work_mem = '64kB';

RESET work_mem;

VACUUM r9s;

SET enable_indexonlyscan = off;

RESET enable_indexonlyscan;

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT id, v FROM r9s WHERE g IN (1, 3, 99) AND v < -26 ORDER BY id DESC LIMIT 20;

RESET enable_seqscan;

CREATE FUNCTION r8f39168(m int, OUT a int, OUT b text) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, t FROM r8a WHERE id <= m ORDER BY id;
  RETURN QUERY EXECUTE 'SELECT $1 * 100, $2' USING m, 'x';
  a := -1; b := NULL; RETURN NEXT;
  IF m < 0 THEN RETURN QUERY SELECT 1, 2; END IF;
END $$;

SELECT * FROM r8f39168(2);

SELECT a, b FROM r8f39168(4) ORDER BY a, b;

SELECT * FROM r8f39168(-1);

DROP FUNCTION r8f39168;

CREATE FUNCTION r9f8271(int, int) RETURNS bigint LANGUAGE sql STABLE AS $f$SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2$f$;

DROP FUNCTION r9f8271;

CREATE TABLE r9a37912 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a37912 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 98, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 98) i;

SET default_statistics_target = 1000;

UPDATE r9a37912 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a37912;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a37912 WHERE s LIKE 'v1%'$$);

EXPLAIN SELECT * FROM r9a37912 WHERE s LIKE 'v1%';

SELECT * FROM r9est($$SELECT * FROM r9a37912 WHERE g = 2 AND s = 'common'$$);

SELECT * FROM r9est($$SELECT * FROM r9a37912 WHERE g = 39$$);

DROP TABLE r9a37912;

CREATE FUNCTION r8f92752(lo int, hi int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE n bigint; r text := ''; got int;
BEGIN
  UPDATE r8a SET v = v WHERE id BETWEEN lo AND hi;
  GET DIAGNOSTICS n = ROW_COUNT;
  r := r || 'upd=' || n || ' found=' || FOUND;
  SELECT id INTO got FROM r8a WHERE id BETWEEN lo AND hi ORDER BY id;
  r := r || ' got=' || coalesce(got::text, 'null') || ' found=' || FOUND;
  PERFORM 1 FROM r8a WHERE false;
  r := r || ' perform=' || FOUND;
  RETURN r;
END $$;

SELECT r8f92752(1, 1);

SELECT r8f92752(1, 3);

SELECT r8f92752(1000, 1001);

DROP FUNCTION r8f92752;

SET enable_hashagg = off;

RESET enable_hashagg;

PREPARE r9p22560 (text) AS SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;

EXECUTE r9p22560('{1,2,3}');

EXECUTE r9p22560('{50,100,NULL}');

EXECUTE r9p22560('{1,2,3}');

-- statement
EXPLAIN (COSTS OFF) EXECUTE r9p22560('{1,2,3}');
