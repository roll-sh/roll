-- class: wrong_rows
-- ordered: false
-- note: 5 vs 5 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Sort
-- engine:         Sort Key: (t) COLLATE "en-x-icu", id
-- engine:         ->  Seq Scan on r9s
-- engine:               Filter: (((t)::text >= 'a'::text) AND ((t)::text <= 'b'::text))
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: t COLLATE "en-x-icu", id
-- reference:         ->  Seq Scan on r9s
-- reference:               Filter: (((t)::text >= 'a'::text) AND ((t)::text <= 'b'::text))
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a interval, b timestamp, c interval);

CREATE INDEX t0_i282 ON t0 (c);

CREATE INDEX t0_i860 ON t0 (c);

CREATE TABLE t1 (a int4, b timestamp, c date, d jsonb, e float8);

INSERT INTO t1 (a, b, c, d, e) VALUES (7, '2024-01-09 06:30:00', '2024-02-06', '1', 4), (5, '2024-01-08 00:00:00', '2024-03-04', '"s"', 4), (8, '2024-01-05 23:00:00', '2024-03-17', 'null', 0.5), (9, '2024-01-06 23:30:00', '2024-01-28', '1', 3), (2, NULL, '2024-03-26', '{"b": [1, "a"]}', NULL);

CREATE INDEX t1_i819 ON t1 (e);

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW(NULL, -1)::ct3, NULL, NULL), (2, ROW('Z', NULL)::ct3, '{}', 3), (3, ROW('9', -1)::ct3, NULL, NULL), (4, ROW('Z', 0)::ct3, ARRAY[ROW(NULL, 3)::ct3, ROW('B', 1)::ct3], 1), (5, NULL, NULL, 0), (6, ROW('x ', 1)::ct3, NULL, NULL), (7, ROW('Ä', 2)::ct3, '{}', NULL), (8, ROW('Z', NULL)::ct3, '{}', 4);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN ('A' < q COLLATE "POSIX" AND 1::numeric <= 0::numeric) THEN RETURN ((12::int8 * p))::int4;
  ELSE RETURN p;
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE sql STABLE AS $$ SELECT id FROM t0 WHERE id <= lim $$;

CREATE VIEW v0 AS SELECT cardinality((ARRAY[[1,2,3],[4,5,6]])[1:3]) AS c0, array_agg(('3 days 04:00:00'::interval)::text ORDER BY ('3 days 04:00:00'::interval)::text) AS c1, percentile_disc(0.5) WITHIN GROUP (ORDER BY mod(trunc(1::numeric), 3)) AS c2, '2024-03-18 08:00:00+00'::timestamptz AS c3 FROM (WITH w2 AS MATERIALIZED (SELECT (32767::int2 + 8::int2) AS c0 FROM t0 t3 WHERE (date_trunc('hour', '2000-02-06 08:00:00-05'::timestamptz) + t3.c) <= (('2025-06-18'::date)::timestamp AT TIME ZONE 'UTC')) SELECT '2025-01-28 07:59:00'::timestamp AS c0, ((('2025-04-13'::date + 3))::timestamp - (('-2 days'::interval * 7) + ('2000-02-15 03:59:00'::timestamp - '2000-09-09 08:59:00'::timestamp))) AS c1 FROM w2 t4) s1 WHERE 0::int8 NOT BETWEEN abs((NULL::int8 + 12::int8)) AND abs(2::int8) GROUP BY s1.c1, (ascii(' x')) HAVING EXISTS (SELECT ((t5.a - length('Z')) / 3) FROM t1 t5);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2025-10-26 01:30:00+00', '2024-02-29 00:00:00', NULL, '1 mon -30 days', 'Infinity', NULL, -3), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '-infinity', '2025-03-09 02:30:00', '-infinity', NULL, '0.5', '-1.25', 6), (3, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', NULL, '1970-01-01 00:00:00.5', '-infinity', '-infinity', '0.000001', '-1.25', -1), (4, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2025-12-31 23:59:59.999999', '2024-02-29', '-1 year -1 day', 'NaN', '0.125', 8), (5, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2025-03-09 03:30:00-04', '0001-01-01 00:00:00 BC', '2024-02-29', 'infinity', '-0.0', '-1.25', 5), (6, '"str"', '"str"', '2025-11-02 01:30:00-05', 'infinity', '1999-12-31', 'P1DT12H', '-1e-20', '1024', 2), (7, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '2025-10-26 02:30:00', '2000-03-01', '2 weeks 3 microseconds', '0', '-1.25', 5), (8, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2025-10-26 01:30:00+00', '2025-03-09 02:30:00', 'infinity', '-infinity', '99999999999999999999', '2', NULL), (9, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '4713-01-01 BC', 'infinity', NULL, '2', 3), (10, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '2000-01-01 00:00:00', '2024-02-29', '36 hours', 'Infinity', NULL, NULL), (11, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', 'infinity', '2025-12-31 23:59:59.999999', '4713-01-01 BC', '1 day', '-7', 'Infinity', 5), (12, '{}', '{}', '2024-02-29 23:00:00+00', NULL, 'infinity', '-infinity', '-Infinity', '0.5', 7), (13, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-04-06 02:30:00+11', '2025-11-02 01:30:00', '2025-01-01', '1 day', 'NaN', '2', 4);

INSERT INTO tp4 VALUES (7, '2000-03-01', NULL, '0.5'), (22, '2024-02-29', 'a', '-1e-20'), (17, NULL, NULL, NULL), (3, '1999-12-31', 'c', '0.5');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5ci", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 11, 'i', '7', '0', '[-3,0]', '{}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (2, 18, '', '-99999999999999999999999999999999999.999999', '-2147483648 days', '(,)', '{(,0],[10,)}', NULL, ARRAY[ROW(NULL, 'å')::r5ct]), (3, 5, 'ab', 'NaN', 'infinity', '(,)', '{(,0],[10,)}', '', ARRAY[ROW(0, 'B')::r5ct]), (4, 0, 'o', '-1e40', '178000000 years', '[-3,0]', '{[1,2],[2,3)}', '', ARRAY[ROW(4, 'ö')::r5ct, ROW(0, 'Ab')::r5ct]), (5, 10, 'I', 'Infinity', '2147483647 days', 'empty', '{[2,4)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(3, '9')::r5ct, ROW(NULL, 'o')::r5ct, ROW(0, 'ö')::r5ct]), (6, NULL, 'Ä', '0', NULL, '[-3,0]', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(5, 'h')::r5ct, ROW(2, 'é')::r5ct]), (7, 6, '10', '123456789012345678901234567890.123456789', '1 year -1 sec', '(,)', '{(,0],[10,)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(1, NULL)::r5ct, ROW(2, 'Ä')::r5ct]), (8, 6, 'E', '0', '1 mon', '[-3,0]', '{[1,3),[5,7)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(NULL, 'B')::r5ct, ROW(3, 'ä')::r5ct]), (9, 3, 'AB', '0', '0', '(,)', '{[1,3),[5,7)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(1, 'ab')::r5ct]), (10, 8, 'ae', NULL, NULL, '(,)', NULL, NULL, ARRAY[ROW(0, 'ö')::r5ct, ROW(5, 'ch')::r5ct]), (11, 7, 'å', '7', '2147483647 days', '[-3,0]', NULL, 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(3, ' a')::r5ct, ROW(2, NULL)::r5ct, ROW(4, 'b')::r5ct]);

CREATE INDEX r5a_b ON r5a (b COLLATE "en-x-icu");

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 15;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (14, 'a', 'infinity'), (10, 'b', '2024-02-29'), (3, 'c', NULL), (-5, 'd', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (22, 'x0', NULL), (28, 'x1', NULL), (30, 'x2', NULL), (24, 'x3', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, NULL, '{"plan": "pro", "seats": 3}'), (2, 'u2@x.org', '2', 2, '{a,b}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'zoë3', 3, '{a,b}', NULL), (4, 'u4@x.org', 'ann4', 4, '{}', '{"plan": "pro", "seats": 3}'), (5, 'u5@x.org', 'Éva5', 5, '{b,NULL}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 9.99, 'new', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 0, 'void', '2025-02-26', NULL), (4, 100, 'paid', '2025-02-14', NULL), (5, 0, 'void', '2025-02-22', '{"gift": true}'), (3, 100, 'new', '2025-02-10', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 100, 'void', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 0, 'new', '2025-02-12', '{"gift": true}'), (3, -1.50, 'new', '2025-02-16', '{"gift": true}'), (5, 12345678.90, 'new', '2025-02-27', '{"items": []}'), (2, 9.99, 'new', '2025-02-03', NULL), (3, NULL, 'new', '2025-02-07', '{"gift": true}'), (2, 0, 'paid', '2025-02-09', '{"gift": true}'), (2, NULL, 'paid', '2025-02-19', NULL), (1, 12345678.90, 'void', '2025-02-21', NULL), (4, 12345678.90, 'void', '2025-02-18', '{"items": []}'), (3, 100, 'paid', '2025-02-28', '{"items": [{"sku": "a", "qty": 2}]}'), (1, -1.50, 'void', '2025-02-28', '{"gift": true}'), (3, 100, 'new', '2025-02-18', '{"items": []}'), (5, NULL, 'paid', '2025-02-03', '{"gift": true}'), (2, 100, 'paid', '2025-02-01', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 12345678.90, 'void', '2025-02-24', NULL), (1, 12345678.90, 'new', '2025-02-13', NULL), (2, -1.50, 'paid', '2025-02-14', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 9.99, 'void', '2025-02-17', '{"gift": true}'), (4, -1.50, 'paid', '2025-02-14', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 8, 18, 'NOTE 3', -12.125, 0.5, '2025-03-09 07:00:00+00'), (2, 8, 171, 'n%5', -12.125, 2.25, '2024-02-29 12:00:00+00'), (3, 1, 62, 'NOTE 3', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (4, 1, -11, 'Note 3', NULL, -0, '2025-03-09 07:00:00+00'), (5, 1, 128, 'note 2', 0.5, 0.5, '2025-03-09 06:59:59+00'), (6, 1, 83, 'Note 3', 1000, 'NaN', '2025-03-09 06:59:59+00'), (7, 1, 141, 'note_4', 0, -0, '2025-03-09 07:00:00+00'), (8, 1, 92, 'note_4', -12.125, 1e300, '2024-02-29 12:00:00+00'), (9, 1, 194, 'note_4', 0, 2.25, '2025-11-02 05:30:00+00'), (10, 1, NULL, 'note 3', -12.125, 2.25, '2025-11-02 05:30:00+00'), (11, 1, 142, 'é', -12.125, 2.25, '2025-03-09 06:59:59+00'), (12, 1, 63, 'Note 3', NULL, 2.25, '2025-03-09 06:59:59+00'), (13, 9, 56, 'NOTE 3', 1000, 1e300, '2025-03-09 07:00:00+00'), (14, 1, 131, 'note 3', 1000, NULL, NULL), (15, 7, 135, 'é', 3.333, 'Infinity', '2025-03-09 07:00:00+00'), (16, 8, NULL, 'n%5', 3.333, 'Infinity', '2025-11-02 05:30:00+00'), (17, 5, 67, 'é', NULL, 2.25, NULL), (18, 1, 151, 'Note 3', -12.125, 0.5, '2024-02-29 12:00:00+00'), (19, 1, 77, 'NOTE 3', 0.5, NULL, '2025-03-09 06:59:59+00'), (20, 5, 34, 'note 1', 1000, 'NaN', NULL), (21, 1, 181, 'Note 3', NULL, NULL, '2024-02-29 12:00:00+00'), (22, 8, -45, 'NOTE 3', NULL, 2.25, NULL), (23, 1, -23, 'NOTE 3', 0.5, 2.25, '2025-03-09 06:59:59+00'), (24, 9, -36, '', NULL, 2.25, '2024-02-29 12:00:00+00'), (25, 1, NULL, 'note_4', 3.333, 2.25, '2025-11-02 05:30:00+00'), (26, 1, 35, 'note 3', 0, NULL, '2025-03-09 07:00:00+00'), (27, 1, 52, 'note 3', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (28, 2, 169, '', NULL, NULL, '2025-03-09 07:00:00+00'), (29, 1, NULL, 'note_4', 0, 1e300, '2025-03-09 06:59:59+00'), (30, 2, 146, 'Note 3', 0, 'NaN', '2025-11-02 05:30:00+00');

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

INSERT INTO r7t VALUES (1, '9', '9'), (2, 'e', 'e'), (3, 'E', 'E'), (4, 'a', 'a'), (5, 'Æ', 'Æ'), (6, 'ae', 'ae'), (7, 'Æ', 'Æ');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

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

INSERT INTO r8a VALUES (1, 3, 13, -2.25, 'ab', '2025-01-01'::timestamp + interval '120 hours'), (2, 1, NULL, 0, NULL, '2025-01-01'::timestamp + interval '99 hours'), (3, 3, NULL, 10, 'B', '2025-01-01'::timestamp + interval '192 hours'), (4, 3, 3, 1.50, 'a', '2025-01-01'::timestamp + interval '140 hours'), (5, 3, -4, -2.25, 'ab', '2025-01-01'::timestamp + interval '22 hours'), (6, 3, 1, 10, 'B', '2025-01-01'::timestamp + interval '2 hours'), (7, 4, -5, NULL, '', '2025-01-01'::timestamp + interval '10 hours'), (8, 3, 2, 1.50, 'B', '2025-01-01'::timestamp + interval '4 hours'), (9, 4, -1, 0, NULL, '2025-01-01'::timestamp + interval '36 hours'), (10, 2, 2, 3.75, 'b', '2025-01-01'::timestamp + interval '18 hours'), (11, 1, 18, 10, 'z', '2025-01-01'::timestamp + interval '44 hours'), (12, 2, 6, 3.75, 'b', '2025-01-01'::timestamp + interval '4 hours'), (13, 1, NULL, 1.50, 'B', '2025-01-01'::timestamp + interval '57 hours'), (14, 3, -2, NULL, 'z', '2025-01-01'::timestamp + interval '117 hours'), (15, 2, 12, NULL, '', '2025-01-01'::timestamp + interval '29 hours'), (16, 3, 19, 1.50, 'ab', '2025-01-01'::timestamp + interval '24 hours'), (17, 4, 8, 10, 'a', '2025-01-01'::timestamp + interval '104 hours'), (18, 2, 5, NULL, 'ab', '2025-01-01'::timestamp + interval '40 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 1, 6), (2, 6, 4), (4, 3, 4), (6, 4, 8), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[3,7)', '{(,3)}', NULL, '[1.5,2.5)'), (2, NULL, '{(,0)}', '[2025-03-01,infinity)', '[0,0]'), (3, '[2,)', '{(,2)}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (4, '[-3,-3)', '{[-3,-3), [-1, 1)}', '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (5, '[4,)', NULL, '(2024-12-31,2025-01-05]', '[0,0]'), (6, 'empty', NULL, '[2025-01-01,2025-02-01)', '(,0]'), (7, NULL, NULL, '[2025-01-01,2025-02-01)', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'brown''s fox''s den'), (2, 'running rats'), (3, 'über café naïve'), (4, 'jumps over the lazy dog'), (5, 'über café naïve');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2036) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2547) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 4000) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

EXPLAIN (COSTS OFF) SELECT a FROM r9n WHERE a NOT IN (SELECT z FROM r9n WHERE id < 3) ORDER BY a LIMIT 5;

\s2 BEGIN;

\s2 UPDATE r9s SET v = coalesce(v, 0) + 1000 WHERE id BETWEEN 1560 AND 1563 RETURNING id, v;

\s2 INSERT INTO r9s VALUES (11560, 1, 7, 'new', 1.5);

\s2 COMMIT;

CREATE TABLE r9i37236 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i37236 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 40) i;

CREATE INDEX r9i37236_x0 ON r9i37236 USING btree (a);

CREATE INDEX CONCURRENTLY r9i37236_x1 ON r9i37236 USING btree (t text_pattern_ops);

CREATE INDEX r9i37236_x2 ON r9i37236 USING btree (a);

SET enable_seqscan = on;

EXPLAIN (COSTS OFF) SELECT id, a FROM r9i37236 WHERE a < 42 ORDER BY a DESC NULLS FIRST, id LIMIT 10;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i37236 WHERE arr @> '{3}';

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i37236 WHERE arr && '{1,6}';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i37236;

CREATE TABLE r9i65758 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i65758 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3225) i;

CREATE INDEX r9i65758_x0 ON r9i65758 USING gist (p);

CREATE INDEX ON r9i65758 USING btree (abs(a - 40));

CREATE INDEX ON r9i65758 USING brin (a) WITH (pages_per_range = 2);

INSERT INTO r9i65758 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(3226, 3355) i;

UPDATE r9i65758 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i65758 WHERE id % 11 = 0;

REINDEX TABLE r9i65758;

SET enable_seqscan = on;

SET enable_bitmapscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i65758 WHERE j @> '{"k": 2}';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i65758;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = auto; FOR i IN 0..8 LOOP s := s + coalesce((SELECT coalesce(max(v), 0) FROM r9s WHERE g = 99 AND v > 100 / (i - 7)), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

\s3 BEGIN;

\s3 INSERT INTO r7g (a, t) VALUES (5, 'sv');

\s3 SAVEPOINT a;

\s3 INSERT INTO r7g (a, t) VALUES (5, 'sv');

\x3 p=t'0'
SELECT 1 / $1::int;

\s3 SELECT 1;

\s3 ROLLBACK TO a;

\s3 UPDATE r7d SET ps = array_append(ps, 9) WHERE id = 1;

\s3 END;

\s3 ROLLBACK;

\s3 RELEASE x;

\s2 SET plan_cache_mode = force_generic_plan;

\x2 name=r9x88382 types=25 describe p=b42
SELECT id, t FROM r9s WHERE t > $1 ORDER BY t, id LIMIT 4;

\x2 name=r9x88382 rf=1 p=t''
-;

\x2 name=r9x88382 rf=1 p=t''
-;

\x2 name=r9x88382 rf=1,0 p=t'B'
-;

\x2 name=r9x88382 rf=1 p=t'B'
-;

\x2 name=r9x88382 rf=1,0 p=t''
-;

\x2 name=r9x88382 p=b42
-;

\x2 name=r9x88382 rows=2 p=t'ab3'
-;

\x2 name=r9x88382 rf=1,0 p=t'é1'
-;

\x2 name=r9x88382 p=t'z'
-;

\x2 name=r9x88382 rf=0 p=b62
-;

\x2 name=r9x88382 p=t''
-;

\x2 name=r9x88382 rf=1 p=t'a'
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x88382';

\s2 DEALLOCATE r9x88382;

\s2 RESET plan_cache_mode;

-- statement
EXPLAIN (COSTS OFF) SELECT id, t FROM r9s WHERE t COLLATE "en-x-icu" BETWEEN 'a' AND 'b' ORDER BY t COLLATE "en-x-icu", id LIMIT 8;
