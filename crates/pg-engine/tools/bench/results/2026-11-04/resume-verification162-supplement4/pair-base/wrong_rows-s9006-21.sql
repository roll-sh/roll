-- class: wrong_rows
-- ordered: false
-- note: 15 vs 3 rows
-- engine: QUERY PLAN:25
-- engine: Result
-- engine:   InitPlan 1
-- engine:     ->  Limit
-- engine:           ->  Index Only Scan Backward using r9s_pkey on r9s
-- engine:                 Index Cond: (id < 1525)
-- engine:   InitPlan 2
-- engine:     ->  Limit
-- engine:           ->  Index Only Scan using r9s_pkey on r9s r9s_1
-- engine:                 Index Cond: (id < 1525)
-- engine:   InitPlan 3
-- engine:     ->  Limit
-- engine:           ->  Sort
-- engine:                 Sort Key: r9s_2.v DESC
-- engine:                 ->  Seq Scan on r9s r9s_2
-- engine:                       Filter: ((v IS NOT NULL) AND (id < 1525))
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Aggregate
-- reference:   ->  Seq Scan on r9s
-- reference:         Filter: (id < 1525)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a int4, b text[]) PARTITION BY RANGE (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t0_p1 PARTITION OF t0 FOR VALUES FROM (3) TO (6);

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b) VALUES (12, -2, NULL), (26, 7, NULL), (4, 3, NULL);

CREATE INDEX t0_i343 ON t0 (b) WHERE b IS NOT NULL;

CREATE INDEX t0_i5 ON t0 USING hash (id);

CREATE INDEX t0_i392 ON t0 USING hash (id);

CREATE TABLE t1 (id int4 PRIMARY KEY, a numeric(10,2), b int4);

INSERT INTO t1 (id, a, b) VALUES (10, NULL, NULL), (18, NULL, 1), (16, NULL, NULL), (24, 0.05, -1), (39, NULL, NULL), (27, NULL, 8), (20, NULL, NULL), (33, NULL, NULL), (38, NULL, NULL), (9, NULL, NULL), (19, 8.50, 3), (3, NULL, NULL);

CREATE INDEX t1_i711 ON t1 (a);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; NEW.b := coalesce(NEW.b, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t2 (id int4 PRIMARY KEY, a timestamptz, b timestamp, c int4, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t2 (id, a, b, c) VALUES (26, '2024-01-05 23:00:00+00', '2024-01-02 00:30:00', -2), (8, '2024-01-08 06:00:00+00', '2024-01-07 06:30:00', 6), (35, NULL, '2024-01-01 00:00:00', 7);

CREATE INDEX t2_i356 ON t2 (b);

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('Z', 0)::ct3, ARRAY[ROW('x ', 2)::ct3, ROW('Ä', 0)::ct3], 1), (2, ROW('a', 2)::ct3, ARRAY[ROW('ß', 3)::ct3, ROW('a%', 3)::ct3], 0), (3, NULL, '{}', 0), (4, NULL, ARRAY[ROW('aa', 3)::ct3, ROW('B', 3)::ct3], 4), (5, ROW('é', 3)::ct3, ARRAY[ROW('A', 3)::ct3, ROW('aa', NULL)::ct3], NULL), (6, ROW('ba', NULL)::ct3, ARRAY[ROW('x ', 3)::ct3, ROW('9', 3)::ct3], 0), (7, ROW('', 3)::ct3, NULL, 0), (8, ROW('ab', 1)::ct3, ARRAY[ROW('x ', -1)::ct3, ROW('9', 0)::ct3], 2), (9, ROW('Ab', 2)::ct3, ARRAY[ROW('10', NULL)::ct3, ROW('ba', 3)::ct3], 0), (10, ROW('ba', 0)::ct3, ARRAY[ROW(NULL, 0)::ct3, ROW('ab', NULL)::ct3], 4), (11, ROW(NULL, 3)::ct3, '{}', 0);

CREATE INDEX tc3_cd ON tc3 (c DESC NULLS LAST, n);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE sql STABLE AS $$ SELECT id FROM t0 WHERE id <= lim $$;

CREATE VIEW v0 AS SELECT NULLIF((NULL::int8 * t4.id), abs((SELECT count((-3)) FROM t2 t5))) AS c0, (ascii(upper('Ab')))::int2 AS c1, t4.a AS c2, ('{3,1,2}'::int4[] || NULLIF(t4.g, abs(t4.c))) AS c3 FROM ((SELECT ascii(substr('ab', 0, 2)) AS c0, array_to_string('{"x y",z}'::text[], ',', '?') AS c1, (('{}'::json -> 'a'))::text::json AS c2, date_bin('15 days', t2.b, '2000-01-01')::date AS c3 FROM (t2 t2 JOIN t1 t3 ON CASE WHEN '2000-10-02'::date = '2025-07-24'::date THEN true ELSE starts_with('bar', '9') END) WHERE NULL::bool) s1 LEFT JOIN t2 t4 ON s1.c0 = t4.id);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, 'null', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '2025-11-02 01:30:00', '2025-03-09', 'P1DT12H', '0.000001', 'NaN', 8), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-04-06 02:30:00+11', '1970-01-01 00:00:00.5', '-infinity', '1 day', '-0.0', '0.5', 9), (3, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-11-02 01:30:00-04', NULL, '2000-03-01', '2 weeks 3 microseconds', '-2.5', 'NaN', 7), (4, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '2024-02-29 00:00:00', '2025-01-01', NULL, 'Infinity', '2', NULL), (5, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-11-02 01:30:00-05', '0001-01-01 00:00:00 BC', '-infinity', '1 mon -30 days', '123.45', '-3', 11), (6, NULL, NULL, '2025-04-06 02:30:00+11', '2024-02-29 00:00:00', 'infinity', '1 day', '-7', '2', 1), (7, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '0044-03-15 12:00:00+00 BC', '2025-12-31 23:59:59.999999', '4713-01-01 BC', '36 hours', '-7', '-1.25', NULL), (8, NULL, NULL, '1999-12-31 23:59:59.999999+00', '2025-10-26 02:30:00', NULL, 'infinity', NULL, '0.5', 0), (9, '"str"', '"str"', '2025-10-26 00:30:00+00', '1970-01-01 00:00:00.5', NULL, '1 year 2 mons', '1e30', '-1.25', 12), (10, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2038-01-19 03:14:08+00', '0001-01-01 00:00:00 BC', '-infinity', '1 year 2 mons', '1e30', '0.5', 12), (11, '"str"', '"str"', '2025-10-26 00:30:00+00', '2025-12-31 23:59:59.999999', '-infinity', 'infinity', '1.23456789012345678901234567890', '2', 1), (12, '"str"', '"str"', '0044-03-15 12:00:00+00 BC', NULL, 'infinity', '36 hours', '-Infinity', '0.125', 2), (13, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '-infinity', '2025-11-02 01:30:00', '2024-02-29', '1 mon -30 days', '-7', NULL, -1), (14, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '2025-12-31 23:59:59.999999', NULL, '1 day', 'NaN', '0.5', NULL);

INSERT INTO tp4 VALUES (20, 'infinity', 'ab', '0'), (14, '2024-02-29', 'b', '-2.5'), (11, '1999-12-31', 'a', NULL), (10, 'infinity', 'ab', '-0.0'), (3, '4713-01-01 BC', 'd', NULL), (2, '2024-02-29', 'a', '0'), (14, '-infinity', 'a', '-7'), (5, '4713-01-01 BC', 'd', '-1e-20'), (16, '2025-01-01', 'd', '0'), (3, '4713-01-01 BC', 'b', NULL), (21, '-infinity', 'd', '0.000001'), (9, 'infinity', 'c', NULL), (9, '-infinity', 'ab', NULL), (22, '-infinity', 'a', '-2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "ucs_basic", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 6, 'z', '-0.5', '178000000 years', '[0,10)', '{[1,2],[2,3)}', NULL, '{}'), (2, 14, 'i', 'NaN', '-178000000 years', '[-3,0]', '{(,0],[10,)}', NULL, ARRAY[ROW(0, '')::r5ct, ROW(4, 'i')::r5ct, ROW(NULL, 'c')::r5ct]), (3, 4, 'c', '7', '-178000000 years', '(1,5]', '{[0,100)}', 'running runners ran; the runner runs', '{}'), (4, 5, 'İ', '-0.5', '-178000000 years', '(,)', '{[2,4)}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (5, 4, 'a-', '2.5', '178000000 years', '[5,6)', '{empty}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (6, 4, 'ab', '0.0000000000000000000000000001', '2147483647 mons', '[-3,0]', '{[2,4)}', 'the and of', ARRAY[ROW(5, 'e')::r5ct, ROW(4, 'i')::r5ct]), (7, 16, NULL, '7', '720:00:00', '(,)', '{[2,4)}', 'Fat cats! FAT rats? fat-free', NULL), (8, NULL, 'E', '0', '1 mon', '[5,6)', '{[1,2],[2,3)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL), (9, NULL, 'İ', '-99999999999999999999999999999999999.999999', '-178000000 years', '(1,5]', '{}', 'PostgreSQL full text search: lexemes & weights', NULL), (10, 8, 'o', '1e-130', '178000000 years', '[-3,0]', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', '{}');

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 21;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (-3, 'a', NULL), (-4, 'b', '2024-02-29'), (-2, 'c', '2024-02-29'), (13, 'd', '2025-01-01'), (19, 'a', NULL) ON CONFLICT DO NOTHING;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob 1', 1, '{a,b}', '[1, {"a": null}]'), (2, 'u2@x.org', 'bob2', 2, '{b,NULL}', '{"seats": 1.50}'), (3, 'u3@x.org', 'Cid3', 0, '{}', '[1, {"a": null}]'), (4, 'u4@x.org', 'Cid4', 4, NULL, '{"plan": "free", "tags": [1, 2]}'), (5, 'u5@x.org', 'bob 5', 0, NULL, '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', 'ann6', 6, NULL, '{}'), (7, 'u7@x.org', 'dee7', 7, '{a}', '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (6, -1.50, 'paid', '2025-02-14', '{"items": [{"sku": "a", "qty": 2}]}'), (3, NULL, 'new', '2025-02-09', NULL), (7, 9.99, 'void', '2025-02-03', '{"gift": true}'), (6, NULL, 'new', '2025-02-10', NULL), (2, 12345678.90, 'new', '2025-02-21', NULL), (5, 12345678.90, 'paid', '2025-02-06', '{"gift": true}'), (1, NULL, 'new', '2025-02-22', '{"items": []}'), (4, 12345678.90, 'void', '2025-02-17', NULL), (5, 0, 'new', '2025-02-03', NULL);

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

INSERT INTO r7k VALUES (1, 1, -15, 'note 2', -12.125, 'NaN', NULL), (2, 2, 174, '', 0.5, NULL, '2025-03-09 07:00:00+00'), (3, 4, 146, 'NOTE 3', 1000, 'Infinity', '2025-03-09 07:00:00+00'), (4, 1, -31, 'note 1', 1000, -0, '2025-03-09 07:00:00+00'), (5, 1, 105, 'Note 3', 0, 2.25, '2025-03-09 07:00:00+00'), (6, 1, -24, 'note 1', -12.125, 'NaN', NULL), (7, 1, -11, 'note 1', 0.5, 1e300, '2025-03-09 07:00:00+00'), (8, 1, NULL, 'note 2', -12.125, 2.25, NULL), (9, 5, 45, 'Note 3', -12.125, 'NaN', '2025-03-09 07:00:00+00'), (10, 1, 186, '', 0, 'NaN', '2024-02-29 12:00:00+00'), (11, 7, 186, 'note 3', 1000, NULL, '2025-11-02 05:30:00+00'), (12, 1, 21, '', 0.5, NULL, '2024-02-29 12:00:00+00'), (13, 1, NULL, 'n%5', -12.125, 0.5, '2025-11-02 05:30:00+00'), (14, 9, 22, 'NOTE 3', 3.333, 1e300, NULL), (15, 1, 69, 'n%5', -12.125, 'Infinity', '2024-02-29 12:00:00+00'), (16, 4, 90, '', 0, 2.25, '2025-03-09 07:00:00+00'), (17, 1, 106, 'note 2', 0.5, 0.5, '2025-03-09 06:59:59+00'), (18, 1, NULL, 'Note 3', 0, -0, '2024-02-29 12:00:00+00'), (19, 1, 172, 'Note 3', 1000, -0, '2025-03-09 06:59:59+00'), (20, 1, 174, 'n%5', NULL, NULL, NULL), (21, 1, 24, 'note 3', 0, 1e300, '2025-03-09 06:59:59+00'), (22, 1, 127, 'n%5', NULL, 1e300, '2025-11-02 05:30:00+00'), (23, 1, 76, 'note 2', NULL, -0, '2024-02-29 12:00:00+00'), (24, 5, 27, 'Note 3', -12.125, 0.5, '2025-11-02 05:30:00+00'), (25, 4, 116, 'é', 1000, NULL, '2025-11-02 05:30:00+00'), (26, 6, NULL, 'note_4', NULL, 2.25, NULL), (27, 1, -22, 'note 1', 0, 2.25, NULL), (28, 1, 179, '', 1000, -0, '2025-03-09 06:59:59+00'), (29, 8, 66, 'NOTE 3', NULL, 0.5, NULL), (30, 1, -19, 'note 3', 1000, -0, '2025-11-02 05:30:00+00'), (31, 1, NULL, 'n%5', 1000, 2.25, '2025-03-09 07:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10), (9, 'us', 17), (10, 'eu', 1);

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

INSERT INTO r7t VALUES (1, 'ß', 'ß'), (2, '10', '10'), (3, '10', '10'), (4, 'e', 'e'), (5, 'z', 'z'), (6, '', ''), (7, '10', '10');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, 10, 1.50, 'B', '2025-01-01'::timestamp + interval '9 hours'), (2, 3, NULL, 1.50, 'z', '2025-01-01'::timestamp + interval '96 hours'), (3, 4, -2, -2.25, 'b', '2025-01-01'::timestamp + interval '123 hours'), (4, 2, 1, 0, 'b', '2025-01-01'::timestamp + interval '76 hours'), (5, 1, 2, NULL, 'z', '2025-01-01'::timestamp + interval '23 hours'), (6, 3, NULL, 1.50, 'ab', '2025-01-01'::timestamp + interval '96 hours'), (7, 4, 7, 0, NULL, '2025-01-01'::timestamp + interval '177 hours'), (8, 2, 8, 1.50, 'b', '2025-01-01'::timestamp + interval '116 hours'), (9, 4, 1, 0, 'B', '2025-01-01'::timestamp + interval '12 hours'), (10, 1, 1, 3.75, 'b', '2025-01-01'::timestamp + interval '51 hours'), (11, 3, 18, 0, 'a', '2025-01-01'::timestamp + interval '29 hours'), (12, 1, 17, 3.75, 'z', '2025-01-01'::timestamp + interval '13 hours'), (13, 2, 13, 3.75, 'ab', '2025-01-01'::timestamp + interval '9 hours'), (14, 3, NULL, 3.75, 'B', '2025-01-01'::timestamp + interval '18 hours'), (15, 1, 9, 10, '', '2025-01-01'::timestamp + interval '120 hours'), (16, 3, 11, 1.50, 'a', '2025-01-01'::timestamp + interval '8 hours'), (17, 3, NULL, 0, NULL, '2025-01-01'::timestamp + interval '43 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 1, 4), (2, 4, 1), (3, 3, 3), (3, 6, 5), (5, 7, 3), (7, 8, 4), (8, 7, 1), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[10,)', NULL, NULL, NULL), (2, '[3,8)', NULL, NULL, '[1.5,2.5)'), (3, '[0,)', NULL, NULL, NULL), (4, NULL, NULL, NULL, '[1.5,2.5)'), (5, '(3,6]', '{[3,6), [8, 10)}', NULL, '[1.5,2.5)'), (6, '[-2,)', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (7, NULL, '{[-3,0), [2, 4)}', '[2025-01-01,2025-02-01)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'über café naïve'), (2, 'foxes and dogs'), (3, 'foxes and dogs'), (4, ''), (5, 'quick quick slow'), (6, 'über café naïve');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3656) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1817) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2052) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

CREATE FUNCTION r8f95904(a int, b int DEFAULT 10, VARIADIC c int[] DEFAULT '{}') RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN a || '/' || b || '/' || coalesce(array_length(c, 1), 0) || '/' || coalesce(c[1], -1); END $$;

SELECT r8f95904(1);

SELECT r8f95904(1, 2, 3, 4);

SELECT r8f95904(b => 5, a => 1);

SELECT r8f95904(1, 2, VARIADIC '{7,8}');

SELECT r8f95904(1, c => '{}');

SELECT r8f95904(1, 2, VARIADIC NULL);

DROP FUNCTION r8f95904;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = force_custom_plan; FOR i IN 0..10 LOOP s := s + coalesce((SELECT coalesce(max(v), 0) FROM r9s WHERE g = 99 AND v > 100 / (i - 7)), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

CREATE TABLE r9a26139 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a26139 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1904, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1904) i;

SET default_statistics_target = 25;

ANALYZE r9a26139;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT g, count(*) FROM r9a26139 GROUP BY g$$);

SELECT * FROM r9est($$SELECT * FROM r9a26139 WHERE u < 100$$);

DROP TABLE r9a26139;

-- statement
EXPLAIN (COSTS OFF) SELECT max(id), min(id), max(v) FROM r9s WHERE id < 1525;
