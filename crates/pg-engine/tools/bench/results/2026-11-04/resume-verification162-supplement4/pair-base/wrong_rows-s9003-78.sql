-- class: wrong_rows
-- ordered: false
-- note: 9 vs 6 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: ((r9i2722.p <-> '(3,3)'::point)), r9i2722.id
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i2722_1.p <-> '(3,3)'::point)), r9i2722_1.id
-- engine:               ->  Seq Scan on r9i2722_1
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i2722_2.p <-> '(3,3)'::point)), r9i2722_2.id
-- engine:               ->  Seq Scan on r9i2722_2
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: ((r9i2722.p <-> '(3,3)'::point)), r9i2722.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i2722_1
-- reference:               ->  Seq Scan on r9i2722_2
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (a int2, b text[], c jsonb);

INSERT INTO t0 (a, b, c) VALUES (1, '{a,b}', '{"a": 3}'), (3, '{a,b}', NULL), (5, '{b,NULL}', NULL), (6, '{A,a}', '{"b": [1, "a"]}'), (3, '{b,NULL}', '1'), (NULL, '{a,b}', '1'), (6, NULL, '{"b": [1, "a"]}'), (0, '{a,b}', NULL), (NULL, '{a,b}', '{"b": [1, "a"]}'), (3, '{b,NULL}', '{"a": 1, "b": "x"}'), (1, '{A,a}', '{"a": "1"}'), (NULL, '{b,NULL}', '"s"');

CREATE INDEX t0_i945 ON t0 (a);

CREATE INDEX t0_i283 ON t0 (c) WHERE c IS NOT NULL;

CREATE INDEX t0_i648 ON t0 USING hash (c);

CREATE TABLE t1 (id int4 PRIMARY KEY, a text, b text, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t1 (id, a, b) VALUES (15, NULL, NULL), (32, NULL, 'Ab'), (51, NULL, NULL), (8, NULL, NULL), (33, NULL, NULL), (35, 'é', 'ß'), (30, NULL, NULL), (31, NULL, ''), (21, NULL, NULL), (2, NULL, 'a%'), (39, 'a', 'A'), (28, NULL, NULL), (20, NULL, NULL), (19, 'ab', NULL), (3, NULL, NULL), (29, NULL, NULL), (36, NULL, NULL), (16, NULL, 'Z'), (54, NULL, NULL), (23, 'é', 'A'), (18, NULL, NULL), (7, NULL, 'ß'), (1, NULL, NULL), (6, NULL, NULL), (9, '', NULL), (60, 'ab', NULL), (50, NULL, 'Z'), (4, 'abc', ''), (40, NULL, NULL), (42, NULL, NULL);

CREATE INDEX t1_i222 ON t1 (id) WHERE id IS NOT NULL;

CREATE TABLE t2 (id int4, a jsonb, b numeric(10,2), c int4) PARTITION BY RANGE (c);

CREATE TABLE t2_p0 PARTITION OF t2 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t2_p1 PARTITION OF t2 FOR VALUES FROM (3) TO (6);

CREATE TABLE t2_p2 PARTITION OF t2 FOR VALUES FROM (6) TO (MAXVALUE);

INSERT INTO t2 (id, a, b, c) VALUES (12, '{"a": 3}', 9.00, 3);

CREATE INDEX t2_i734 ON t2 USING hash (c);

CREATE INDEX t2_i924 ON t2 (id, a);

CREATE INDEX t2_i6 ON t2 USING gin (a);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE STRICT AS $$ SELECT CASE WHEN true THEN (length(q) / 3) ELSE p END $$;

CREATE VIEW v0 AS SELECT ('1'::json -> 'a') AS c0 FROM (t2 t1 FULL JOIN t1 t2 ON t1.c = t2.id AND t1.c = t2.id AND false) WHERE (COALESCE('2000-03-27'::date, '2025-11-07'::date) <> ('2024-08-23'::date + t1.id) OR false);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, NULL, NULL, '1999-12-31 23:59:59.999999+00', '2025-03-09 02:30:00', '2024-02-29', '-12:00', '1.23456789012345678901234567890', 'NaN', -3), (2, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2025-10-26 01:30:00+00', '1970-01-01 00:00:00.5', '2025-03-09', '-infinity', 'Infinity', '0.125', -3), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-10-26 01:30:00+00', '0001-01-01 00:00:00 BC', NULL, '2 weeks 3 microseconds', '0.000001', 'Infinity', 6), (4, '"str"', '"str"', '2025-10-26 01:30:00+00', 'infinity', NULL, 'P1DT12H', '0.000001', '-0', 1), (5, '{}', '{}', '2025-10-26 01:30:00+00', '2025-03-09 02:30:00', '1999-12-31', NULL, NULL, NULL, 1), (6, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '0001-01-01 00:00:00 BC', NULL, NULL, '2.5', '0.5', 3), (7, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-10-26 01:30:00+00', '0001-01-01 00:00:00 BC', '2025-03-09', '1 mon -30 days', 'NaN', NULL, 0), (8, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2025-12-31 23:59:59.999999', NULL, 'P1DT12H', NULL, NULL, 8), (9, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-11-02 01:30:00-04', NULL, '1999-12-31', '2 weeks 3 microseconds', '99999999999999999999', 'Infinity', 12), (10, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-10-26 01:30:00+00', '2024-02-29 00:00:00', '2024-02-29', 'infinity', '-Infinity', '-3', 5), (11, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', 'infinity', NULL, '2025-01-01', '-1 mon 2 days', '0', NULL, 11), (12, NULL, NULL, NULL, '0001-01-01 00:00:00 BC', '-infinity', '1 year 2 mons', 'NaN', '-3', NULL), (13, '{}', '{}', '2024-02-29 23:00:00+00', '2025-10-26 02:30:00', '2025-03-09', '1 day', '2.5', '-1.25', NULL), (14, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, NULL, 'infinity', 'P1DT12H', '1.23456789012345678901234567890', NULL, 4);

INSERT INTO tp4 VALUES (1, 'infinity', 'd', NULL), (1, '1999-12-31', 'c', '99999999999999999999'), (21, '-infinity', NULL, NULL), (-2, '4713-01-01 BC', NULL, NULL), (0, NULL, 'a', '123.45'), (24, '1999-12-31', 'b', NULL), (6, '2024-02-29', 'ab', '0.5'), (4, '2025-03-09', NULL, '-2.5'), (20, '4713-01-01 BC', 'ab', 'Infinity'), (8, '2025-01-01', 'b', NULL), (-4, '2025-01-01', 'b', '1.23456789012345678901234567890'), (3, '2024-02-29', 'd', '-2.5'), (10, '1999-12-31', 'd', '1.23456789012345678901234567890'), (0, '2024-02-29', 'b', '-Infinity');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, -1, 'Ä', '7', '720:00:00', '[1,5)', '{[1,2],[2,3)}', 'the and of', ARRAY[ROW(-1, NULL)::r5ct, ROW(5, 'ch')::r5ct]), (2, NULL, 'AB', '0.0000000000000000000000000001', '2147483647 mons', '(1,5]', NULL, 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', '{}'), (3, -3, 'I', '-1e40', '2147483647 days', '[1,5)', '{[2,4)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(0, 'h')::r5ct, ROW(3, 'ä')::r5ct, ROW(-1, 'ö')::r5ct]), (4, 9, 'A', '-Infinity', '1 day 00:00:00.000001', '(,)', '{[1,3),[5,7)}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (5, NULL, 'Z', '-0.5', '1 day 00:00:00.000001', '[-3,0]', '{[1,2],[2,3)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(NULL, 'Ä')::r5ct, ROW(2, '9')::r5ct, ROW(3, 'AB')::r5ct]), (6, 18, 'ß', '-Infinity', '720:00:00', '(,)', '{[2,4)}', 'PostgreSQL full text search: lexemes & weights', '{}');

CREATE INDEX r5a_b ON r5a (b COLLATE "tr-x-icu");

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 28 WITH CHECK OPTION;

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

INSERT INTO r5p VALUES (8, 'a', NULL), (17, 'b', NULL), (-3, 'c', '2025-01-01'), (-2, 'd', NULL), (15, 'a', NULL), (10, 'b', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (23, 'x0', NULL), (28, 'x1', NULL), (21, 'x2', NULL), (27, 'x3', NULL), (20, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Éva1', 1, '{a}', NULL), (2, 'u2@x.org', 'Éva2', 2, NULL, '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'ann3', 3, '{a}', '{"seats": 1.50}'), (4, 'u4@x.org', 'zoë4', 0, '{}', '{"plan": "free", "tags": [1, 2]}'), (5, 'u5@x.org', 'bob5', 5, '{b,NULL}', '{"seats": 1.50}'), (6, 'u6@x.org', 'bob 6', 0, NULL, '[1, {"a": null}]'), (7, 'u7@x.org', 'zoë7', 7, '{a}', NULL), (8, 'u8@x.org', 'Cid8', 8, '{b,NULL}', '{"seats": 1.50}'), (9, 'u9@x.org', '9', 9, '{}', '{"plan": "pro", "seats": 3}');

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

INSERT INTO r7k VALUES (1, 1, 48, 'n%5', NULL, NULL, '2025-03-09 07:00:00+00'), (2, 1, 34, 'é', 0, 2.25, NULL), (3, 8, 173, 'é', NULL, -0, NULL), (4, 8, 119, 'note 1', 3.333, -0, '2025-03-09 06:59:59+00'), (5, 1, 146, '', 3.333, NULL, '2025-11-02 05:30:00+00'), (6, 1, 27, 'note 1', 3.333, 'Infinity', '2024-02-29 12:00:00+00'), (7, 2, NULL, 'note 3', 0, 'NaN', '2025-03-09 07:00:00+00'), (8, 1, -12, 'n%5', 1000, 0.5, '2025-11-02 05:30:00+00'), (9, 3, 22, 'n%5', NULL, -0, '2025-03-09 06:59:59+00'), (10, 3, 129, 'note_4', -12.125, 0.5, '2025-03-09 07:00:00+00'), (11, 5, NULL, 'n%5', 0.5, 2.25, '2025-03-09 06:59:59+00'), (12, 1, 92, 'Note 3', NULL, 2.25, '2024-02-29 12:00:00+00'), (13, 6, 134, 'Note 3', NULL, 'Infinity', NULL), (14, 1, 17, 'Note 3', 3.333, 0.5, '2025-03-09 07:00:00+00'), (15, 1, 15, 'note 1', 1000, NULL, NULL), (16, 5, NULL, 'NOTE 3', 3.333, 'Infinity', NULL), (17, 1, 88, 'note 3', 3.333, 0.5, '2025-03-09 06:59:59+00'), (18, 1, 187, 'note_4', NULL, 'NaN', '2025-11-02 05:30:00+00'), (19, 6, 75, 'note_4', -12.125, 2.25, '2024-02-29 12:00:00+00'), (20, 1, -23, 'note 1', 3.333, -0, '2025-03-09 07:00:00+00'), (21, 8, 36, 'note_4', -12.125, -0, '2024-02-29 12:00:00+00'), (22, 1, -11, 'note_4', 3.333, 'Infinity', NULL);

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

CREATE TABLE r7g (id int GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, '10', '10'), (2, 'E', 'E'), (3, 'Ä', 'Ä'), (4, 'E', 'E'), (5, 'Ä', 'Ä'), (6, 'Æ', 'Æ'), (7, 'e', 'e'), (8, 'a', 'a'), (9, 'b', 'b'), (10, '', ''), (11, 'é', 'é'), (12, 'Ä', 'Ä'), (13, 'b', 'b'), (14, 'E', 'E');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 4, -4, -2.25, 'B', '2025-01-01'::timestamp + interval '57 hours'), (2, 2, NULL, 1.50, '', '2025-01-01'::timestamp + interval '58 hours'), (3, 2, -5, 10, 'a', '2025-01-01'::timestamp + interval '57 hours'), (4, 1, 7, -2.25, 'z', '2025-01-01'::timestamp + interval '28 hours'), (5, 1, -2, 10, 'ab', '2025-01-01'::timestamp + interval '106 hours'), (6, 4, 7, 10, 'a', '2025-01-01'::timestamp + interval '24 hours'), (7, 1, 17, NULL, 'ab', '2025-01-01'::timestamp + interval '1 hours'), (8, 4, 10, NULL, 'a', '2025-01-01'::timestamp + interval '140 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 4, 6), (1, 5, 8), (2, 1, 3), (3, 1, 1), (4, 3, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (2, NULL, '{[3,9), [11, 13)}', 'empty', '[0,0]'), (3, '[4,)', '{[4,8), [10, 12)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (4, NULL, '{(,-2)}', '[2025-03-01,infinity)', '[0,0]'), (5, '[3,)', '{[3,4), [6, 8)}', '(2024-12-31,2025-01-05]', NULL), (6, '[6,12)', NULL, NULL, '(1.25,1.5)'), (7, 'empty', '{}', '[2025-01-01,2025-02-01)', NULL), (8, '(-2,0]', '{[-2,0), [2, 4)}', 'empty', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'quick quick slow'), (2, 'dog-eat-dog world'), (3, 'brown''s fox''s den'), (4, ''), (5, 'foxes and dogs'), (6, 'running rats'), (7, 'dog-eat-dog world'), (8, 'brown''s fox''s den');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (4, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2615) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2101) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2050) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r8t68832 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t68832 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf68832() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t68832', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf68832_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t68832', r);
  RETURN NULL;
END $$;

CREATE TRIGGER d_auof AFTER UPDATE OF v ON r8t68832 FOR EACH ROW WHEN (OLD.v IS DISTINCT FROM NEW.v) EXECUTE FUNCTION r8tf68832('of');

CREATE TRIGGER j_bad AFTER INSERT ON r8t68832 REFERENCING OLD TABLE AS ot FOR EACH ROW EXECUTE FUNCTION r8tf68832_tt();

CREATE TRIGGER a_bir BEFORE INSERT OR UPDATE ON r8t68832 FOR EACH ROW EXECUTE FUNCTION r8tf68832('none');

CREATE TRIGGER c_bus BEFORE UPDATE OR DELETE ON r8t68832 FOR EACH STATEMENT EXECUTE FUNCTION r8tf68832('stmt');

CREATE TRIGGER d_auof AFTER UPDATE OF v ON r8t68832 FOR EACH ROW WHEN (OLD.v IS DISTINCT FROM NEW.v) EXECUTE FUNCTION r8tf68832('of');

INSERT INTO r8t68832 VALUES (10, -2, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't68832';

UPDATE r8t68832 SET v = NULL WHERE id = 1 RETURNING id, v, s;

DELETE FROM r8log WHERE tg = 't68832';

INSERT INTO r8t68832 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t68832.v RETURNING *;

DELETE FROM r8log WHERE tg = 't68832';

DELETE FROM r8t68832 WHERE id = 3 RETURNING *;

DELETE FROM r8log WHERE tg = 't68832';

UPDATE r8t68832 SET v = coalesce(v, 0) - 1 RETURNING id, v;

DELETE FROM r8log WHERE tg = 't68832';

INSERT INTO r8t68832 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't68832';

TRUNCATE r8t68832;

DELETE FROM r8log WHERE tg = 't68832';

DROP TABLE r8t68832;

DROP FUNCTION r8tf68832, r8tf68832_tt;

BEGIN;

DECLARE r5k SCROLL WITH HOLD CURSOR FOR SELECT id, a, b FROM r5a ORDER BY id;

FETCH 2 FROM r5k;

SAVEPOINT r5s1;

INSERT INTO r5a (id, a) VALUES (50, 50);

DECLARE r5k2 CURSOR FOR SELECT id, a FROM r5a ORDER BY id;

FETCH 1 FROM r5k2;

ROLLBACK TO r5s1;

FETCH BACKWARD 2 FROM r5k;

FETCH PRIOR FROM r5k;

MOVE FORWARD ALL IN r5k2;

SELECT name, statement, is_holdable, is_binary, is_scrollable OR statement NOT LIKE '%SCROLL%' FROM pg_cursors ORDER BY name;

COMMIT AND CHAIN;

FETCH ALL FROM r5k;

COMMIT;

CLOSE ALL;

CREATE TABLE r9a72981 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a72981 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4143, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4143) i;

ALTER TABLE r9a72981 ALTER COLUMN arr SET STATISTICS 100;

CREATE STATISTICS r9a72981_st (ndistinct, dependencies) ON g, (u % 10) FROM r9a72981;

UPDATE r9a72981 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a72981;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a72981 WHERE g = 1 AND u % 10 = 1$$);

DROP TABLE r9a72981;

CREATE TABLE r9i2722 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i2722_1 PARTITION OF r9i2722 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i2722_2 PARTITION OF r9i2722 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i2722 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3836) i;

CREATE INDEX ON r9i2722 USING spgist (r);

CREATE INDEX r9i2722_x1 ON r9i2722 USING brin (id);

INSERT INTO r9i2722 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(3837, 4006) i;

UPDATE r9i2722 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i2722 WHERE id % 11 = 0;

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i2722 WHERE a = 5;

-- statement
EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i2722 ORDER BY p <-> point '(3,3)', id LIMIT 12;
