-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: r9est:25
-- engine: HashAggregate  (rows=1800)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a18613  (rows=5813)
-- engine: -- SELECT 3
-- reference: r9est:25
-- reference: HashAggregate  (rows=5813)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a18613  (rows=5813)
-- reference: -- SELECT 3

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4[], b jsonb);

INSERT INTO t0 (id, a, b) VALUES (28, '{3,1,2}', '{"a": {"b": 2}}');

CREATE INDEX t0_i921 ON t0 (a);

CREATE INDEX t0_i982 ON t0 ((id + 1));

CREATE INDEX t0_i422 ON t0 (id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a interval, b jsonb, c text, d json, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

CREATE INDEX t1_i639 ON t1 ((id + 1));

CREATE INDEX t1_i473 ON t1 (lower(c));

CREATE INDEX t1_i446 ON t1 USING hash (id);

CREATE TABLE t2 (a timestamptz, b int2);

INSERT INTO t2 (a, b) VALUES ('2024-01-07 12:00:00+00', 5), (NULL, 3), ('2024-01-09 06:00:00+00', 4), ('2024-01-01 12:00:00+00', 0), (NULL, 0), ('2024-01-03 12:00:00+00', 2), ('2024-01-06 00:00:00+00', NULL), ('2024-01-07 00:00:00+00', 6), ('2024-01-01 12:00:00+00', 0), ('2024-01-04 06:00:00+00', 2), ('2024-01-02 00:00:00+00', NULL), ('2024-01-02 06:00:00+00', 3), ('2024-01-02 23:00:00+00', 1), (NULL, 2), ('2024-01-02 06:00:00+00', 3), ('2024-01-06 06:00:00+00', NULL), ('2024-01-07 00:00:00+00', 4), ('2024-01-01 06:00:00+00', NULL), ('2024-01-07 12:00:00+00', 4), ('2024-01-08 23:00:00+00', NULL), ('2024-01-09 00:00:00+00', 4), ('2024-01-06 00:00:00+00', 0), ('2024-01-04 00:00:00+00', 4), ('2024-01-08 23:00:00+00', NULL), ('2024-01-07 00:00:00+00', NULL), ('2024-01-08 23:00:00+00', 4), ('2024-01-05 23:00:00+00', 2), ('2024-01-02 23:00:00+00', 4), ('2024-01-09 00:00:00+00', 0), ('2024-01-04 00:00:00+00', 4);

CREATE INDEX t2_i790 ON t2 (a);

CREATE INDEX t2_i776 ON t2 (b);

CREATE INDEX t2_i889 ON t2 (b) WHERE b IS NOT NULL;

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('z', -1)::ct3, NULL, 1), (2, NULL, ARRAY[ROW('a,b', 3)::ct3, ROW('Z', 2)::ct3], 4), (3, ROW('', 3)::ct3, ARRAY[ROW('z', -1)::ct3, ROW('ba', NULL)::ct3], NULL), (4, ROW('foo', 0)::ct3, NULL, 0), (5, ROW('a%', 3)::ct3, ARRAY[ROW('B', -1)::ct3, ROW(NULL, 0)::ct3], NULL), (6, ROW('Ab', NULL)::ct3, NULL, NULL), (7, ROW('z', -1)::ct3, NULL, 4), (8, ROW('x ', 0)::ct3, '{}', 3), (9, ROW('B', NULL)::ct3, ARRAY[ROW('a%', -1)::ct3, ROW('b', NULL)::ct3], NULL), (10, ROW(' x', 0)::ct3, NULL, 0);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN (SELECT jsonb_agg(t1.id ORDER BY t1.id) FROM t1 t1 WHERE true) @> ('{}'::jsonb - 'a') THEN p ELSE (NULLIF(7::int8, 7::int8))::int4 END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, c FROM t1 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, c FROM t1 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT t2.g AS c0, justify_hours(CASE WHEN true THEN t2.a ELSE CASE WHEN false THEN t2.a ELSE '00:30:00'::interval END END) AS c1, to_jsonb(lower(t2.c)) AS c2, CASE WHEN isfinite('1999-11-26'::date) THEN ((true) IS NOT TRUE AND NULL::jsonb @> t1.b) ELSE EXISTS (SELECT (COALESCE('{3,1,2}'::int4[], (NULL::int4[] || '{3,1,2}'::int4[])))[1] FROM ONLY t2 t3 WHERE to_char(trunc('NaN'::numeric), 'L999') <> 'ba') END AS c3 FROM t1 t1 JOIN t1 t2 USING (b);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2025-03-09 02:30:00', '2025-03-09', '-infinity', '1e30', '0.125', 10), (2, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', '1970-01-01 00:00:00.5', '4713-01-01 BC', '1 mon -30 days', '2.5', NULL, -2), (3, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '4713-01-01 BC', '1 mon -30 days', '0', 'NaN', NULL), (4, 'null', 'null', '2025-03-09 01:30:00-05', '2025-11-02 01:30:00', '4713-01-01 BC', 'P1DT12H', NULL, '1024', 11), (5, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', NULL, '2025-01-01', '0', '99999999999999999999', '-0', NULL), (6, 'null', 'null', '2025-10-26 00:30:00+00', '2025-11-02 01:30:00', '2000-03-01', '23:59:59.999999', NULL, 'Infinity', 9), (7, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '1999-12-31 23:59:59.999999+00', '2025-11-02 01:30:00', 'infinity', '-1 mon 2 days', '-0.0', 'NaN', NULL), (8, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2038-01-19 03:14:08+00', '1970-01-01 00:00:00.5', '-infinity', '23:59:59.999999', '-2.5', NULL, NULL), (9, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '2024-02-29', '-1 mon 2 days', '-Infinity', '0.5', 5), (10, '"str"', '"str"', '0044-03-15 12:00:00+00 BC', '1970-01-01 00:00:00.5', 'infinity', 'infinity', '-1e-20', 'NaN', NULL), (11, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', NULL, '2025-12-31 23:59:59.999999', '2025-01-01', 'P1DT12H', '-1e-20', '-0', NULL), (12, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '0001-01-01 00:00:00 BC', '2024-02-29', 'infinity', '-0.0', 'NaN', 8), (13, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2025-03-09 01:30:00-05', '2024-02-29 00:00:00', NULL, '1 year 2 mons', '-0.0', '-0', NULL);

INSERT INTO tp4 VALUES (14, '2025-03-09', 'ab', '-Infinity'), (-1, '2000-03-01', 'a', 'NaN'), (13, '2000-03-01', 'c', '0.000001'), (23, '2025-01-01', 'c', '0.5'), (2, '2025-01-01', 'ab', '123.45'), (5, '2024-02-29', NULL, '1e30'), (16, '2025-03-09', 'd', '0.5'), (18, '2025-03-09', NULL, NULL), (19, '2000-03-01', 'c', '1.23456789012345678901234567890'), (9, 'infinity', 'ab', NULL), (-3, '2024-02-29', 'a', '1e30');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "en-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, NULL, 'ae', '2.5', '2147483647 mons', '[3,)', '{[2,4)}', 'the and of', ARRAY[ROW(NULL, 'B')::r5ct, ROW(2, 'ß')::r5ct]), (2, NULL, 'AB', '-0.5', '30 days', '[1,5)', NULL, '', NULL), (3, 0, 'a', '-1e40', '2147483647 days', '(1,5]', '{(,0],[10,)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(2, 'ß')::r5ct, ROW(5, 'ö')::r5ct]), (4, NULL, 'e', NULL, '178000000 years', '(,)', '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(2, 'c')::r5ct, ROW(5, 'o')::r5ct, ROW(4, 'c')::r5ct]), (5, NULL, 'ss', '-0.5', NULL, '[1,5)', '{(,0],[10,)}', 'a fat cat sat on a mat and ate a fat rat', NULL), (6, 8, 'ab', '1e-130', '2147483647 days', '(,2]', '{[1,2],[2,3)}', NULL, '{}');

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 18 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (0, 'a', NULL), (8, 'b', '2025-01-01'), (17, 'c', 'infinity'), (-1, 'd', '2025-01-01'), (17, 'a', 'infinity'), (2, 'b', 'infinity'), (15, 'c', 'infinity'), (11, 'd', NULL) ON CONFLICT DO NOTHING;

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, '{a}', '{}'), (2, 'u2@x.org', 'Éva2', 0, NULL, NULL), (3, 'u3@x.org', 'Cid3', 3, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', '4', 4, '{b,NULL}', '{}'), (5, 'u5@x.org', 'bob5', 5, '{b,NULL}', '[1, {"a": null}]'), (6, 'u6@x.org', 'ann6', 6, '{}', NULL), (7, 'u7@x.org', 'Cid7', 7, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (8, 'u8@x.org', 'Éva8', 0, '{a,b}', '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, 12345678.90, 'paid', '2025-02-11', '{"items": []}'), (2, 0, 'void', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 0, 'new', '2025-02-11', '{"items": []}'), (1, -1.50, 'new', '2025-02-03', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 100, 'new', '2025-02-12', NULL), (1, NULL, 'paid', '2025-02-01', '{"items": []}'), (5, 100, 'paid', '2025-02-26', NULL), (2, 0, 'void', '2025-02-21', '{"items": []}'), (4, 100, 'paid', '2025-02-13', '{"items": []}'), (4, NULL, 'new', '2025-02-22', '{"items": []}'), (5, 100, 'void', '2025-02-02', '{"items": [{"sku": "a", "qty": 2}]}'), (2, NULL, 'new', '2025-02-16', NULL), (4, 12345678.90, 'new', '2025-02-23', '{"items": []}'), (3, 100, 'void', '2025-02-20', NULL), (1, 0, 'void', '2025-02-28', '{"items": []}'), (5, 9.99, 'paid', '2025-02-16', '{"gift": true}'), (7, 12345678.90, 'paid', '2025-02-25', '{"gift": true}'), (5, 9.99, 'paid', '2025-02-17', '{"items": [{"sku": "a", "qty": 2}]}'), (6, 9.99, 'new', '2025-02-26', '{"items": []}'), (3, 9.99, 'new', '2025-02-11', '{"items": []}'), (1, 0, 'new', '2025-02-26', '{"items": []}'), (5, NULL, 'new', '2025-02-05', NULL), (6, -1.50, 'new', '2025-02-23', NULL);

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

INSERT INTO r7k VALUES (1, 1, -39, 'note 3', 3.333, 'NaN', '2024-02-29 12:00:00+00'), (2, 1, 45, 'note 2', 1000, 'Infinity', '2025-03-09 07:00:00+00'), (3, 1, 116, 'Note 3', 3.333, 2.25, '2025-11-02 05:30:00+00'), (4, 1, 9, 'é', NULL, NULL, '2025-03-09 06:59:59+00'), (5, 1, 185, 'note 3', 0, 'Infinity', NULL), (6, 1, -19, 'note 2', -12.125, -0, '2025-03-09 07:00:00+00'), (7, 1, 165, 'note 1', 0.5, 1e300, NULL), (8, 7, 68, 'note 1', 0, 0.5, '2025-03-09 07:00:00+00'), (9, 3, 24, 'Note 3', 0, NULL, '2025-11-02 05:30:00+00'), (10, 9, 128, 'Note 3', NULL, NULL, NULL), (11, 1, 191, 'Note 3', NULL, 2.25, '2024-02-29 12:00:00+00'), (12, 1, 13, 'n%5', -12.125, 'NaN', '2025-03-09 06:59:59+00'), (13, 6, 17, '', 0, -0, '2025-03-09 06:59:59+00'), (14, 5, NULL, 'note 2', -12.125, 2.25, '2025-03-09 07:00:00+00'), (15, 1, 125, '', 3.333, 2.25, NULL), (16, 7, -49, '', 0, 'Infinity', NULL), (17, 7, 82, 'é', 0, 2.25, '2025-03-09 07:00:00+00'), (18, 1, 81, 'NOTE 3', 1000, 1e300, '2025-11-02 05:30:00+00'), (19, 1, 81, 'note 2', 0, 0.5, '2025-11-02 05:30:00+00'), (20, 1, 91, 'é', NULL, 'Infinity', NULL), (21, 1, NULL, 'note_4', 0.5, 2.25, '2025-03-09 07:00:00+00'), (22, 1, 138, 'NOTE 3', NULL, 'Infinity', '2024-02-29 12:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10), (9, 'us', 17), (10, 'eu', 1);

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

INSERT INTO r7t VALUES (1, 'z', 'z'), (2, 'E', 'E'), (3, 'ä', 'ä'), (4, 'b', 'b'), (5, 'ä', 'ä');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 4, -1, NULL, '', '2025-01-01'::timestamp + interval '25 hours'), (2, 1, NULL, 1.50, '', '2025-01-01'::timestamp + interval '9 hours'), (3, 4, 4, NULL, 'b', '2025-01-01'::timestamp + interval '7 hours'), (4, 3, 11, -2.25, 'B', '2025-01-01'::timestamp + interval '48 hours'), (5, 2, 16, -2.25, NULL, '2025-01-01'::timestamp + interval '159 hours'), (6, 3, 16, 10, '', '2025-01-01'::timestamp + interval '180 hours'), (7, 1, 0, 0, NULL, '2025-01-01'::timestamp + interval '72 hours'), (8, 1, 8, NULL, 'z', '2025-01-01'::timestamp + interval '24 hours'), (9, 4, 20, 1.50, NULL, '2025-01-01'::timestamp + interval '52 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 3, 7), (3, 7, 6), (4, 1, 9), (5, 6, 3), (5, 3, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{}', '[2025-01-01,2025-02-01)', '(,0]'), (2, NULL, '{}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (3, 'empty', '{(,3)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (4, 'empty', NULL, NULL, '(1.25,1.5)'), (5, '[2,)', NULL, '[2025-03-01,infinity)', '(1.25,1.5)'), (6, '[-2,-1)', '{}', '[2025-03-01,infinity)', NULL), (7, NULL, '{(,5)}', 'empty', '[1.5,2.5)'), (8, '[9,)', NULL, '[2025-03-01,infinity)', '[1.5,2.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, ''), (3, 'a rat ran'), (4, 'dog-eat-dog world'), (5, 'dog-eat-dog world'), (6, 'brown''s fox''s den');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3205) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1828) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2561) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

SELECT '{}'::int4multirange >= '{(,0), [10,)}'::int4multirange, '{}'::int4multirange + '{(,0), [10,)}'::int4multirange, '{}'::int4multirange >= int4range(1, 5, '[]'), int4range(1, 5, '[]') >= '{(,0), [10,)}'::int4multirange, range_merge('{}'::int4multirange), lower('{}'::int4multirange), upper('{(,0), [10,)}'::int4multirange), isempty('{}'::int4multirange), '{}'::int4multirange @> 4;

CREATE TABLE r9c80137 (id int PRIMARY KEY, a int, b text, s serial) WITH (autovacuum_enabled = off);

CREATE UNIQUE INDEX r9c80137_b ON r9c80137 (b);

INSERT INTO r9c80137 (id, a, b) VALUES (7, 1, 'pre'), (1200, 2, 'pre2');

BEGIN;

COPY r9c80137 (id, a, b) FROM STDIN (FORMAT csv);
1,2,b1
2,9,b2
3,16,b3
4,23,b4
5,30,b5
6,37,b6
7,44,b7
8,1,b8
9,8,b9
10,15,b10
11,22,b11
12,29,b12
13,36,b13
14,43,b14
15,0,b15
16,7,b16
17,14,
18,21,b18
19,28,b19
20,35,b20
21,42,b21
22,-1,b22
23,6,b23
24,13,b24
25,20,b25
26,27,b26
27,34,b27
28,41,b28
29,-2,b29
30,5,b30
31,12,b31
32,19,b32
33,26,b33
34,33,
35,40,b35
36,-3,b36
37,4,b37
38,11,b38
39,18,b39
40,25,b40
41,32,b41
42,39,b42
43,-4,b43
44,3,b44
45,10,b45
46,17,b46
47,24,b47
48,31,b48
49,38,b49
50,-5,b50
51,2,
52,9,b52
53,16,b53
54,23,b54
55,30,b55
56,37,b56
57,44,b57
58,1,b58
59,8,b59
60,15,b60
61,22,b61
62,29,b62
63,36,b63
64,43,b64
65,0,b65
66,7,b66
67,14,b67
68,21,
69,28,b69
70,35,b70
71,42,b71
72,-1,b72
73,6,b73
74,13,b74
75,20,b75
76,27,b76
77,34,b77
78,41,b78
79,-2,b79
80,5,b80
81,12,b81
82,19,b82
83,26,b83
84,33,b84
85,40,
86,-3,b86
87,4,b87
88,11,b88
89,18,b89
90,25,b90
91,32,b91
92,39,b92
93,-4,b93
94,3,b94
95,10,b95
96,17,b96
97,24,b97
98,31,b98
99,38,b99
100,-5,b100
101,2,b101
102,9,
103,16,b103
104,23,b104
105,30,b105
106,37,b106
107,44,b107
108,1,b108
109,8,b109
110,15,b110
111,22,b111
112,29,b112
113,36,b113
114,43,b114
115,0,b115
116,7,b116
117,14,b117
118,21,b118
119,28,
120,35,b120
121,42,b121
122,-1,b122
123,6,b123
124,13,b124
125,20,b125
126,27,b126
127,34,b127
128,41,b128
129,-2,b129
130,5,b130
131,12,b131
132,19,b132
133,26,b133
134,33,b134
135,40,b135
136,-3,
137,4,b137
138,11,b138
139,18,b139
140,25,b140
141,32,b141
142,39,b142
143,-4,b143
144,3,b144
145,10,b145
146,17,b146
147,24,b147
148,31,b148
149,38,b149
150,-5,b150
151,2,b151
152,9,b152
153,16,
154,23,b154
155,30,b155
156,37,b156
157,44,b157
158,1,b158
159,8,b159
160,15,b160
161,22,b161
162,29,b162
163,36,b163
164,43,b164
165,0,b165
166,7,b166
167,14,b167
168,21,b168
169,28,b169
170,35,
171,42,b171
172,-1,b172
173,6,b173
174,13,b174
175,20,b175
176,27,b176
177,34,b177
178,41,b178
179,-2,b179
180,5,b180
181,12,b181
182,19,b182
183,26,b183
184,33,b184
185,40,b185
186,-3,b186
187,4,
188,11,b188
189,18,b189
190,25,b190
191,32,b191
192,39,b192
193,-4,b193
194,3,b194
195,10,b195
196,17,b196
197,24,b197
198,31,b198
199,38,b199
200,-5,b200
201,2,b201
202,9,b202
203,16,b203
204,23,
205,30,b205
206,37,b206
207,44,b207
208,1,b208
209,8,b209
210,15,b210
211,22,b211
212,29,b212
213,36,b213
214,43,b214
215,0,b215
216,7,b216
217,14,b217
218,21,b218
219,28,b219
220,35,b220
221,42,
222,-1,b222
223,6,b223
224,13,b224
225,20,b225
226,27,b226
227,34,b227
228,41,b228
229,-2,b229
230,5,b230
231,12,b231
232,19,b232
233,26,b233
234,33,b234
235,40,b235
236,-3,b236
237,4,b237
238,11,
239,18,b239
240,25,b240
241,32,b241
242,39,b242
243,-4,b243
244,3,b244
245,10,b245
246,17,b246
247,24,b247
248,31,b248
249,38,b249
250,-5,b250
251,2,b251
252,9,b252
253,16,b253
254,23,b254
255,30,
256,37,b256
257,44,b257
258,1,b258
259,8,b259
260,15,b260
261,22,b261
262,29,b262
263,36,b263
264,43,b264
265,0,b265
266,7,b266
267,14,b267
268,21,b268
269,28,b269
270,35,b270
271,42,b271
272,-1,
273,6,b273
274,13,b274
275,20,b275
276,27,b276
277,34,b277
278,41,b278
279,-2,b279
280,5,b280
281,12,b281
282,19,b282
283,26,b283
284,33,b284
285,40,b285
286,-3,b286
287,4,b287
288,11,b288
289,18,
290,25,b290
291,32,b291
292,39,b292
293,-4,b293
294,3,b294
295,10,b295
296,17,b296
297,24,b297
298,31,b298
299,38,b299
300,-5,b300
301,2,b301
302,9,b302
303,16,b303
304,23,b304
305,30,b305
306,37,
307,44,b307
308,1,b308
309,8,b309
310,15,b310
311,22,b311
312,29,b312
313,36,b313
314,43,b314
315,0,b315
316,7,b316
317,14,b317
318,21,b318
319,28,b319
320,35,b320
321,42,b321
322,-1,b322
323,6,
324,13,b324
325,20,b325
326,27,b326
327,34,b327
328,41,b328
329,-2,b329
330,5,b330
331,12,b331
332,19,b332
333,26,b333
334,33,b334
335,40,b335
336,-3,b336
337,4,b337
338,11,b338
339,18,b339
340,25,
341,32,b341
342,39,b342
343,-4,b343
344,3,b344
345,10,b345
346,17,b346
347,24,b347
348,31,b348
349,38,b349
350,-5,b350
351,2,b351
352,9,b352
353,16,b353
354,23,b354
355,30,b355
356,37,b356
357,44,
358,1,b358
359,8,b359
360,15,b360
361,22,b361
362,29,b362
363,36,b363
364,43,b364
365,0,b365
366,7,b366
367,14,b367
368,21,b368
369,28,b369
370,35,b370
371,42,b371
372,-1,b372
373,6,b373
374,13,
375,20,b375
376,27,b376
377,34,b377
378,41,b378
379,-2,b379
380,5,b380
381,12,b381
382,19,b382
383,26,b383
384,33,b384
385,40,b385
386,-3,b386
387,4,b387
388,11,b388
389,18,b389
390,25,b390
391,32,
392,39,b392
393,-4,b393
394,3,b394
395,10,b395
396,17,b396
397,24,b397
398,31,b398
399,38,b399
400,-5,b400
401,2,b401
402,9,b402
403,16,b403
404,23,b404
405,30,b405
406,37,b406
407,44,b407
408,1,
409,8,b409
410,15,b410
411,22,b411
412,29,b412
413,36,b413
414,43,b414
415,0,b415
416,7,b416
417,14,b417
418,21,b418
419,28,b419
420,35,b420
421,42,b421
422,-1,b422
423,6,b423
424,13,b424
425,20,
426,27,b426
427,34,b427
428,41,b428
429,-2,b429
430,5,b430
431,12,b431
432,19,b432
433,26,b433
434,33,b434
435,40,b435
436,-3,b436
437,4,b437
438,11,b438
439,18,b439
440,25,b440
441,32,b441
442,39,
443,-4,b443
444,3,b444
445,10,b445
446,17,b446
447,24,b447
448,31,b448
449,38,b449
450,-5,b450
451,2,b451
452,9,b452
453,16,b453
454,23,b454
455,30,b455
456,37,b456
457,44,b457
458,1,b458
459,8,
460,15,b460
461,22,b461
462,29,b462
463,36,b463
464,43,b464
465,0,b465
466,7,b466
467,14,b467
468,21,b468
469,28,b469
470,35,b470
471,42,b471
472,-1,b472
473,6,b473
474,13,b474
475,20,b475
476,27,
477,34,b477
478,41,b478
479,-2,b479
480,5,b480
481,12,b481
482,19,b482
483,26,b483
484,33,b484
485,40,b485
486,-3,b486
487,4,b487
488,11,b488
489,18,b489
490,25,b490
491,32,b491
492,39,b492
493,-4,
494,3,b494
495,10,b495
496,17,b496
497,24,b497
498,31,b498
499,38,b499
500,-5,b500
501,2,b501
502,9,b502
503,16,b503
504,23,b504
505,30,b505
506,37,b506
507,44,b507
508,1,b508
509,8,b509
510,15,
511,22,b511
512,29,b512
513,36,b513
514,43,b514
515,0,b515
516,7,b516
517,14,b517
518,21,b518
519,28,b519
520,35,b520
521,42,b521
522,-1,b522
523,6,b523
524,13,b524
525,20,b525
526,27,b526
527,34,
528,41,b528
529,-2,b529
530,5,b530
531,12,b531
532,19,b532
533,26,b533
534,33,b534
535,40,b535
536,-3,b536
537,4,b537
538,11,b538
539,18,b539
540,25,b540
541,32,b541
542,39,b542
543,-4,b543
544,3,
545,10,b545
546,17,b546
547,24,b547
548,31,b548
549,38,b549
550,-5,b550
551,2,b551
552,9,b552
553,16,b553
554,23,b554
555,30,b555
556,37,b556
557,44,b557
558,1,b558
559,8,b559
560,15,b560
561,22,
562,29,b562
563,36,b563
564,43,b564
565,0,b565
566,7,b566
567,14,b567
568,21,b568
569,28,b569
570,35,b570
571,42,b571
572,-1,b572
573,6,b573
574,13,b574
575,20,b575
576,27,b576
577,34,b577
578,41,
579,-2,b579
580,5,b580
581,12,b581
582,19,b582
583,26,b583
584,33,b584
585,40,b585
586,-3,b586
587,4,b587
588,11,b588
589,18,b589
590,25,b590
591,32,b591
592,39,b592
593,-4,b593
594,3,b594
595,10,
596,17,b596
597,24,b597
598,31,b598
599,38,b599
600,-5,b600
601,2,b601
602,9,b602
603,16,b603
604,23,b604
605,30,b605
606,37,b606
607,44,b607
608,1,b608
609,8,b609
610,15,b610
611,22,b611
612,29,
613,36,b613
614,43,b614
615,0,b615
616,7,b616
617,14,b617
618,21,b618
619,28,b619
620,35,b620
621,42,b621
622,-1,b622
623,6,b623
624,13,b624
625,20,b625
626,27,b626
627,34,b627
628,41,b628
629,-2,
630,5,b630
631,12,b631
632,19,b632
633,26,b633
634,33,b634
635,40,b635
636,-3,b636
637,4,b637
638,11,b638
639,18,b639
640,25,b640
641,32,b641
642,39,b642
643,-4,b643
644,3,b644
645,10,b645
646,17,
647,24,b647
648,31,b648
649,38,b649
650,-5,b650
651,2,b651
652,9,b652
653,16,b653
654,23,b654
655,30,b655
656,37,b656
657,44,b657
658,1,b658
659,8,b659
660,15,b660
661,22,b661
662,29,b662
663,36,
664,43,b664
665,0,b665
666,7,b666
667,14,b667
668,21,b668
669,28,b669
670,35,b670
671,42,b671
672,-1,b672
673,6,b673
674,13,b674
675,20,b675
676,27,b676
677,34,b677
678,41,b678
679,-2,b679
680,5,
681,12,b681
682,19,b682
683,26,b683
684,33,b684
685,40,b685
686,-3,b686
687,4,b687
688,11,b688
689,18,b689
690,25,b690
691,32,b691
692,39,b692
693,-4,b693
694,3,b694
695,10,b695
696,17,b696
697,24,
698,31,b698
699,38,b699
700,-5,b700
701,2,b701
702,9,b702
703,16,b703
704,23,b704
705,30,b705
706,37,b706
707,44,b707
708,1,b708
709,8,b709
710,15,b710
711,22,b711
712,29,b712
713,36,b713
714,43,
715,0,b715
716,7,b716
717,14,b717
718,21,b718
719,28,b719
720,35,b720
721,42,b721
722,-1,b722
723,6,b723
724,13,b724
725,20,b725
726,27,b726
727,34,b727
728,41,b728
729,-2,b729
730,5,b730
731,12,
732,19,b732
733,26,b733
734,33,b734
735,40,b735
736,-3,b736
737,4,b737
738,11,b738
739,18,b739
740,25,b740
741,32,b741
742,39,b742
743,-4,b743
744,3,b744
745,10,b745
746,17,b746
747,24,b747
748,31,
749,38,b749
750,-5,b750
751,2,b751
752,9,b752
753,16,b753
754,23,b754
755,30,b755
756,37,b756
757,44,b757
758,1,b758
759,8,b759
760,15,b760
761,22,b761
762,29,b762
763,36,b763
764,43,b764
765,0,
766,7,b766
767,14,b767
768,21,b768
769,28,b769
770,35,b770
771,42,b771
772,-1,b772
773,6,b773
774,13,b774
775,20,b775
776,27,b776
777,34,b777
778,41,b778
779,-2,b779
780,5,b780
781,12,b781
782,19,
783,26,b783
784,33,b784
785,40,b785
786,-3,b786
787,4,b787
788,11,b788
789,18,b789
790,25,b790
791,32,b791
792,39,b792
793,-4,b793
794,3,b794
795,10,b795
796,17,b796
797,24,b797
798,31,b798
799,38,
800,-5,b800
801,2,b801
802,9,b802
803,16,b803
804,23,b804
805,30,b805
806,37,b806
807,44,b807
808,1,b808
809,8,b809
810,15,b810
811,22,b811
812,29,b812
813,36,b813
814,43,b814
815,0,b815
816,7,
817,14,b817
818,21,b818
819,28,b819
820,35,b820
821,42,b821
822,-1,b822
823,6,b823
824,13,b824
825,20,b825
826,27,b826
827,34,b827
828,41,b828
829,-2,b829
830,5,b830
831,12,b831
832,19,b832
833,26,
834,33,b834
835,40,b835
836,-3,b836
837,4,b837
838,11,b838
839,18,b839
840,25,b840
841,32,b841
842,39,b842
843,-4,b843
844,3,b844
845,10,b845
846,17,b846
847,24,b847
848,31,b848
849,38,b849
850,-5,
851,2,b851
852,9,b852
853,16,b853
854,23,b854
855,30,b855
856,37,b856
857,44,b857
858,1,b858
859,8,b859
860,15,b860
861,22,b861
862,29,b862
863,36,b863
864,43,b864
865,0,b865
866,7,b866
867,14,
868,21,b868
869,28,b869
870,35,b870
871,42,b871
872,-1,b872
873,6,b873
874,13,b874
875,20,b875
876,27,b876
877,34,b877
878,41,b878
879,-2,b879
880,5,b880
881,12,b881
882,19,b882
883,26,b883
884,33,
885,40,b885
886,-3,b886
887,4,b887
888,11,b888
889,18,b889
890,25,b890
891,32,b891
892,39,b892
893,-4,b893
894,3,b894
895,10,b895
896,17,b896
897,24,b897
898,31,b898
899,38,b899
900,-5,b900
901,2,
902,9,b902
903,16,b903
904,23,b904
905,30,b905
906,37,b906
907,44,b907
908,1,b908
909,8,b909
910,15,b910
911,22,b911
912,29,b912
913,36,b913
914,43,b914
915,0,b915
916,7,b916
917,14,b917
918,21,
919,28,b919
920,35,b920
921,42,b921
922,-1,b922
923,6,b923
924,13,b924
925,20,b925
926,27,b926
927,34,b927
928,41,b928
929,-2,b929
930,5,b930
931,12,b931
932,19,b932
933,26,b933
934,33,b934
935,40,
936,-3,b936
937,4,b937
938,11,b938
939,18,b939
940,25,b940
941,32,b941
942,39,b942
943,-4,b943
944,3,b944
945,10,b945
946,17,b946
947,24,b947
948,31,b948
949,38,b949
950,-5,b950
951,2,b951
952,9,
953,16,b953
954,23,b954
955,30,b955
956,37,b956
957,44,b957
958,1,b958
959,8,b959
960,15,b960
961,22,b961
962,29,b962
963,36,b963
964,43,b964
965,0,b965
966,7,b966
967,14,b967
968,21,b968
969,28,
970,35,b970
971,42,b971
972,-1,b972
973,6,b973
974,13,b974
975,20,b975
976,27,b976
977,34,b977
978,41,b978
979,-2,b979
980,5,b980
981,12,b981
982,19,b982
983,26,b983
984,33,b984
985,40,b985
986,-3,
987,4,b987
988,11,b988
989,18,b989
990,25,b990
991,32,b991
992,39,b992
993,-4,b993
994,3,b994
995,10,b995
996,17,b996
997,24,b997
998,31,b998
999,38,b999
1000,-5,b1000
1001,2,b1001
1002,9,b1002
1003,16,
1004,23,b1004
1005,30,b1005
1006,37,b1006
1007,44,b1007
1008,1,b1008
1009,8,b1009
1010,15,b1010
1011,22,b1011
1012,29,b1012
\.

\s2& INSERT INTO r9c80137 (id, a, b) VALUES (818, 0, 'w818') ON CONFLICT DO NOTHING;

ROLLBACK;

\s2.;

DROP TABLE r9c80137;

SET TimeZone = 'UTC';

SET extra_float_digits = 1;

\s3 DEALLOCATE r7n12794;

SELECT numnode(to_tsquery('simple', 'brown <-> fox's')), querytree(to_tsquery('simple', 'brown <-> fox's')), to_tsquery('simple', 'brown <-> fox's') && 'x'::tsquery, !! 'a'::tsquery, 'a'::tsquery <-> 'b <-> c'::tsquery, tsquery_phrase('a', 'b', 3), ts_rewrite('a & b'::tsquery, 'a'::tsquery, 'c | d'::tsquery), 'fat:ab & cat'::tsquery @> 'cat'::tsquery;

CREATE FUNCTION r8f92222(a int, b int DEFAULT 10, VARIADIC c int[] DEFAULT '{}') RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN a || '/' || b || '/' || coalesce(array_length(c, 1), 0) || '/' || coalesce(c[1], -1); END $$;

SELECT r8f92222(1);

SELECT r8f92222(1, 2, 3, 4);

SELECT r8f92222(b => 5, a => 1);

SELECT r8f92222(1, 2, VARIADIC '{7,8}');

SELECT r8f92222(1, c => '{}');

SELECT r8f92222(1, 2, VARIADIC NULL);

DROP FUNCTION r8f92222;

SELECT numnode(to_tsquery('english', 'fox <-> den')), querytree(to_tsquery('english', 'fox <-> den')), to_tsquery('english', 'fox <-> den') && 'x'::tsquery, !! 'a'::tsquery, 'a'::tsquery <-> 'b <-> c'::tsquery, tsquery_phrase('a', 'b', 3), ts_rewrite('a & b'::tsquery, 'a'::tsquery, 'c | d'::tsquery), 'fat:ab & cat'::tsquery @> 'cat'::tsquery;

CREATE TABLE r9a18613 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a18613 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 5813, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 5813) i;

CREATE STATISTICS r9a18613_st ON g, u, b FROM r9a18613;

UPDATE r9a18613 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a18613;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a18613 WHERE g = 3$$);

-- statement
SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a18613$$);
