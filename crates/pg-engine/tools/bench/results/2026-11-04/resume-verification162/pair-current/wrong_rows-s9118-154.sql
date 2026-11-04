-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: r9est:25
-- engine: HashAggregate  (rows=1600)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a25130  (rows=3906)
-- engine: -- SELECT 3
-- reference: r9est:25
-- reference: HashAggregate  (rows=558)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a25130  (rows=3906)
-- reference: -- SELECT 3

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a jsonb, b float8, c timestamptz, d json, e interval);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (38, 'null', NULL, '2024-01-01 23:00:00+00', NULL, NULL), (30, NULL, 2.25, '2024-01-03 00:00:00+00', NULL, '0'), (27, '{"a": 3}', NULL, '2024-01-05 00:00:00+00', NULL, NULL), (18, '{"b": [1, "a"]}', 0.5, NULL, NULL, '1 day'), (29, NULL, 2, '2024-01-05 12:00:00+00', '{"b": [1, "a"]}', NULL), (12, '[1, 2, 3]', -1.5, '2024-01-02 00:00:00+00', NULL, '24:00:00'), (9, '{"a": {"b": 2}}', NULL, '2024-01-07 00:00:00+00', '[1, 2, 3]', '30 days'), (34, NULL, NULL, '2024-01-01 06:00:00+00', NULL, NULL), (15, 'null', NULL, NULL, '[1, 2, 3]', '0'), (36, NULL, 0.75, NULL, '{"a": 1, "b": "x"}', '1 mon'), (25, NULL, NULL, '2024-01-06 06:00:00+00', '{"a": 3}', '2 hours'), (32, NULL, NULL, '2024-01-02 06:00:00+00', '1', NULL), (2, NULL, -2, '2024-01-07 06:00:00+00', 'null', '30 days'), (40, '1', -1.25, '2024-01-02 00:00:00+00', 'null', '0'), (21, '[]', 3.25, '2024-01-03 00:00:00+00', '{"b": [1, "a"]}', '1 mon'), (3, '{"a": 1, "b": "x"}', 0.75, '2024-01-07 00:00:00+00', '1', '0'), (1, '{"a": 1, "b": "x"}', 0.5, '2024-01-01 23:00:00+00', NULL, NULL), (37, '{"a": 3}', NULL, '2024-01-08 12:00:00+00', NULL, '30 days'), (49, 'null', -2, '2024-01-07 06:00:00+00', NULL, '0'), (59, 'null', 1, '2024-01-09 23:00:00+00', NULL, '2 hours');

CREATE INDEX t0_i526 ON t0 (id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a numeric(10,2), b text[], c int4, d text);

INSERT INTO t1 (id, a, b, c, d) VALUES (21, NULL, '{}', NULL, NULL);

CREATE INDEX t1_i465 ON t1 (c);

CREATE INDEX t1_i321 ON t1 (c);

CREATE INDEX t1_i713 ON t1 USING hash (id);

CREATE TABLE t2 (id int4 PRIMARY KEY, a int8, b int4, c int4[], d bool);

INSERT INTO t2 (id, a, b, c, d) VALUES (29, 10, NULL, NULL, NULL), (1, NULL, NULL, '{1,1,2}', false), (23, NULL, 8, '{3,1,2}', NULL), (26, NULL, 3, NULL, false), (10, 6, 2, NULL, NULL), (35, 11, -1, '{3,1,2}', true), (25, NULL, 7, '{3,1,2}', false), (18, NULL, 5, NULL, true), (19, 15, 3, '{5}', true), (17, 6, 0, NULL, false), (3, 14, NULL, '{1,2}', true), (9, -2, NULL, '{1,2}', NULL);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int4, b text);

INSERT INTO t3 (id, a, b) VALUES (20, 8, 'z'), (25, NULL, ' x'), (37, 0, '10'), (9, 9, 'Ab'), (55, 2, 'ba');

CREATE INDEX t3_i89 ON t3 (b);

CREATE INDEX t3_i801 ON t3 (b);

CREATE INDEX t3_i714 ON t3 USING hash (id);

CREATE TYPE ct3 AS (a text COLLATE "C", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, NULL, ARRAY[ROW(NULL, 3)::ct3, ROW('9', 2)::ct3], NULL), (2, ROW(NULL, 3)::ct3, NULL, 0), (3, ROW('Ab', 3)::ct3, ARRAY[ROW('a_b', NULL)::ct3, ROW('abc', 1)::ct3], 1);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE VIEW v0 AS SELECT (abs((3::int8 * (-2147483648))) * (ARRAY[(-5), 8])[1]) AS c0 FROM ((SELECT date_trunc('day', '2000-12-10 01:00:00+02:30'::timestamptz) AS c0, GREATEST(string_to_array(upper('x '), ','), '{a,b}'::text[]) AS c1, 0.5::float8 AS c2 FROM t3 t2 WHERE starts_with(t2.b, format('%s|%s', t2.b, position(t2.b in t2.b)))) s1 RIGHT JOIN ((SELECT 'infinity'::date AS c0, ('1999-05-15'::date)::text::date AS c1, GREATEST(0::numeric, ceil(3::numeric)) = 1.5::numeric AS c2 FROM (t0 t4 JOIN t2 t5 ON t4.id = t5.id AND t4.id <> t5.id)) UNION (SELECT (('2025-02-25 03:59:00'::timestamp)::date + extract(day from ('2000-01-01'::date - t6.id))::int) AS c0, NULLIF('infinity'::date, ('2025-04-07'::date + (NULL::date - '1999-04-08'::date))) AS c1, false AS c2 FROM (t0 t6 JOIN t1 t7 ON t6.id = t7.id))) s3 ON '{3,1,2}'::int4[] @> '{3,1,2}'::int4[]) WHERE 99.99::numeric < SOME (SELECT 12345678.9::numeric FROM ONLY t1 t8);

CREATE VIEW v1 AS SELECT ('{2,NULL}'::int4[])[1] AS c0, (mod(round(s1.v0, 2), 3) * s1.v0) AS c1, s1.v1 AS c2, (SELECT (- t3.id) FROM t0 t3 ORDER BY 1 LIMIT 1) AS c3 FROM ((VALUES ('NaN'::numeric, 6, '2025-05-04'::date), (3::numeric, 10, '2024-01-25'::date), (NULL, 9, '2000-09-11'::date), ('NaN'::numeric, 10, '2000-10-01'::date)) s1(v0, v1, v2) JOIN unnest('{3,1,2}'::int4[]) WITH ORDINALITY s2(u, o) ON s1.v1 = s2.u) WHERE true;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2038-01-19 03:14:08+00', '2025-10-26 02:30:00', '2000-03-01', '1 year 2 mons', NULL, '-3', 3), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '1970-01-01 00:00:00.5', '2000-03-01', NULL, '-2.5', 'Infinity', -1), (3, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '0001-01-01 00:00:00 BC', 'infinity', '2 weeks 3 microseconds', '1.23456789012345678901234567890', '1024', 12), (4, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-11-02 01:30:00-04', 'infinity', '1999-12-31', '-12:00', '-0.0', '1024', -3), (5, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '-infinity', '2025-10-26 02:30:00', NULL, '1 year 2 mons', '-1e-20', '2', 10);

INSERT INTO tp4 VALUES (19, '2024-02-29', 'a', 'NaN'), (23, '2024-02-29', 'ab', NULL), (0, '-infinity', 'd', '-0.0'), (9, '1999-12-31', 'b', '-0.0'), (19, 'infinity', NULL, 'NaN'), (-2, '2025-01-01', NULL, NULL), (23, '2000-03-01', 'b', '-0.0'), (12, '4713-01-01 BC', 'd', NULL), (4, '2025-01-01', 'd', '-7'), (18, '2025-03-09', 'd', NULL), (6, NULL, 'ab', '1e30'), (17, '2025-03-09', 'b', '-7'), (-5, '2000-03-01', 'c', '0'), (-2, '2000-03-01', 'a', 'NaN'), (6, 'infinity', 'b', '-7'), (0, '2024-02-29', 'ab', '123.45');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5num", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 18, 'å', NULL, 'infinity', '[3,)', '{(,0],[10,)}', NULL, NULL), (2, 1, 'ö', '1e-130', '1 mon', '[5,6)', '{(,0],[10,)}', 'running runners ran; the runner runs', '{}'), (3, 8, 'Z', '1e-130', '-infinity', '(,2]', '{empty}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(5, 'e')::r5ct]), (4, 6, 'Ä', '1e100', '-1 mon 30 days', '[5,6)', '{[0,100)}', '', ARRAY[ROW(5, 'Z')::r5ct, ROW(4, 'ı')::r5ct, ROW(2, '')::r5ct]), (5, 14, '', '-99999999999999999999999999999999999.999999', '-2147483648 days', '(,)', '{(,0],[10,)}', '', ARRAY[ROW(4, 'ı')::r5ct]), (6, 4, 'é', '-99999999999999999999999999999999999.999999', '-infinity', 'empty', '{[1,2],[2,3)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', '{}');

CREATE INDEX r5a_iv ON r5a (iv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -1 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 18 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (0, 'a', '2025-01-01'), (18, 'b', NULL), (12, 'c', '2024-02-29') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (22, 'x0', NULL), (29, 'x1', NULL), (23, 'x2', NULL), (31, 'x3', NULL), (32, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, '{b,NULL}', NULL), (2, 'u2@x.org', 'dee2', 0, NULL, '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'Cid3', 3, '{b,NULL}', '{}'), (4, 'u4@x.org', 'bob 4', 4, '{b,NULL}', '[1, {"a": null}]'), (5, 'u5@x.org', 'bob 5', 5, '{a}', '{"seats": 1.50}'), (6, 'u6@x.org', 'bob6', 6, '{b,NULL}', '{"seats": 1.50}'), (7, 'u7@x.org', 'ann7', 0, '{a,b}', '[1, {"a": null}]'), (8, 'u8@x.org', 'zoë8', 0, '{a}', '{}'), (9, 'u9@x.org', '9', 9, '{a}', NULL), (10, 'u10@x.org', 'bob10', 0, '{a}', '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (5, -1.50, 'new', '2025-02-26', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 0, 'void', '2025-02-21', '{"items": [{"sku": "a", "qty": 2}]}'), (8, NULL, 'void', '2025-02-27', '{"items": [{"sku": "a", "qty": 2}]}'), (10, NULL, 'new', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (7, 9.99, 'new', '2025-02-06', '{"items": [{"sku": "a", "qty": 2}]}'), (6, NULL, 'void', '2025-02-21', '{"items": []}'), (8, 12345678.90, 'void', '2025-02-17', '{"gift": true}'), (6, 12345678.90, 'paid', '2025-02-05', '{"gift": true}'), (4, 9.99, 'paid', '2025-02-08', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 0, 'new', '2025-02-13', '{"items": [{"sku": "a", "qty": 2}]}'), (9, 100, 'new', '2025-02-27', '{"gift": true}'), (10, NULL, 'new', '2025-02-02', '{"items": []}'), (8, 0, 'void', '2025-02-09', NULL), (5, -1.50, 'paid', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 100, 'void', '2025-02-17', NULL), (6, 12345678.90, 'paid', '2025-02-23', NULL), (1, NULL, 'void', '2025-02-01', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 9.99, 'paid', '2025-02-11', '{"items": [{"sku": "a", "qty": 2}]}');

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

INSERT INTO r7k VALUES (1, 1, 51, 'note 1', NULL, 2.25, '2025-11-02 05:30:00+00'), (2, 1, 115, 'note 3', 0, 0.5, '2025-11-02 05:30:00+00'), (3, 1, -43, 'note 1', 0, 'Infinity', '2025-11-02 05:30:00+00'), (4, 1, 111, 'é', 0, 'NaN', NULL), (5, 8, 103, 'note 1', 0.5, 0.5, '2025-03-09 06:59:59+00'), (6, 7, 180, 'Note 3', 3.333, 'NaN', NULL), (7, 1, 127, 'note 3', 3.333, 0.5, '2025-11-02 05:30:00+00'), (8, 1, 178, 'note 2', -12.125, 'Infinity', '2025-11-02 05:30:00+00'), (9, 1, 157, '', 0.5, 'Infinity', '2025-03-09 07:00:00+00'), (10, 1, -19, '', NULL, 2.25, '2025-03-09 06:59:59+00'), (11, 1, 93, 'note 3', NULL, 'NaN', '2025-03-09 06:59:59+00'), (12, 3, 19, 'NOTE 3', NULL, 0.5, '2024-02-29 12:00:00+00'), (13, 2, 83, 'é', 3.333, 'Infinity', '2024-02-29 12:00:00+00'), (14, 1, 51, 'n%5', 0.5, 1e300, NULL), (15, 2, NULL, 'NOTE 3', -12.125, 1e300, NULL), (16, 6, -22, 'n%5', NULL, 0.5, NULL), (17, 1, 46, 'Note 3', 1000, -0, NULL), (18, 1, 172, 'note 3', 1000, 1e300, '2024-02-29 12:00:00+00'), (19, 1, 103, 'note 3', 1000, 0.5, NULL), (20, 1, 175, 'n%5', -12.125, NULL, '2024-02-29 12:00:00+00'), (21, 4, 39, 'n%5', 0, 2.25, NULL), (22, 1, 56, 'note 3', 0.5, 'NaN', '2025-03-09 06:59:59+00'), (23, 7, 129, 'note 2', 3.333, -0, '2024-02-29 12:00:00+00');

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

INSERT INTO r7t VALUES (1, 'Æ', 'Æ'), (2, 'Æ', 'Æ'), (3, 'ae', 'ae'), (4, '9', '9'), (5, 'ä b', 'ä b'), (6, '10', '10'), (7, 'A', 'A'), (8, 'ß', 'ß'), (9, 'b', 'b'), (10, 'A', 'A'), (11, 'z', 'z'), (12, 'ß', 'ß');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, 9, 3.75, 'a', '2025-01-01'::timestamp + interval '13 hours'), (2, 4, -2, 0, 'z', '2025-01-01'::timestamp + interval '24 hours'), (3, 3, 10, 10, 'b', '2025-01-01'::timestamp + interval '48 hours'), (4, 1, NULL, 1.50, 'b', '2025-01-01'::timestamp + interval '183 hours'), (5, 2, 8, 10, 'B', '2025-01-01'::timestamp + interval '34 hours'), (6, 3, NULL, -2.25, 'B', '2025-01-01'::timestamp + interval '142 hours'), (7, 3, NULL, 10, 'B', '2025-01-01'::timestamp + interval '87 hours'), (8, 1, -5, NULL, 'b', '2025-01-01'::timestamp + interval '26 hours'), (9, 1, 12, NULL, 'b', '2025-01-01'::timestamp + interval '70 hours'), (10, 4, 16, 0, 'a', '2025-01-01'::timestamp + interval '40 hours'), (11, 3, NULL, 1.50, '', '2025-01-01'::timestamp + interval '180 hours'), (12, 3, NULL, NULL, 'ab', '2025-01-01'::timestamp + interval '120 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 1, 1), (3, 4, 9), (4, 3, 5), (4, 4, 4), (7, 1, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{(,10)}', '[2025-03-01,infinity)', '(,0]'), (2, 'empty', '{}', '[2025-03-01,infinity)', '[0,0]'), (3, '[5,)', '{[5,10), [12, 14)}', NULL, '(,0]'), (4, NULL, '{}', 'empty', '(,0]'), (5, '[0,2)', '{(,0)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (6, 'empty', '{}', '[2025-03-01,infinity)', '(1.25,1.5)'), (7, 'empty', '{[-3,3), [5, 7)}', '[2025-01-01,2025-02-01)', '(1.25,1.5)'), (8, '(-1,2]', '{(,-1)}', NULL, NULL);

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, ''), (3, 'the quick brown fox'), (4, 'über café naïve'), (5, 'brown''s fox''s den'), (6, '');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '"scalar"'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '"scalar"'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (5, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3346) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2214) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2725) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

PREPARE r9p49526 (int, int) AS SELECT id, v FROM r9s WHERE g = $1 AND v > $2 ORDER BY v, id LIMIT 5;

EXECUTE r9p49526(1, -1);

EXPLAIN (COSTS OFF) EXECUTE r9p49526(2, -1);

EXECUTE r9p49526(2, -1);

EXECUTE r9p49526(1, 3000000000);

EXECUTE r9p49526(3, -1);

EXECUTE r9p49526(1, NULL);

EXPLAIN (COSTS OFF) EXECUTE r9p49526(1, 3000000000);

EXECUTE r9p49526(1, 3000000000);

EXECUTE r9p49526(2, 0);

EXPLAIN (COSTS OFF) EXECUTE r9p49526(1, -50);

EXECUTE r9p49526(1, -50);

EXECUTE r9p49526(2, 7);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p49526';

DEALLOCATE r9p49526;

RESET plan_cache_mode;

CREATE TABLE r9w89866 (a int, b text);

INSERT INTO r9w89866 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q89866 AS SELECT a FROM r9w89866 WHERE a >= $1 ORDER BY a;

EXECUTE r9q89866(1);

EXECUTE r9q89866(2);

ALTER TABLE r9w89866 ALTER COLUMN a SET DEFAULT 5;

EXECUTE r9q89866(1);

DEALLOCATE r9q89866;

DROP TABLE r9w89866;

\s2 SET plan_cache_mode = force_generic_plan;

\x2 name=r9x75405 types=23 describe rf=0 p=t'0'
SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

\x2 name=r9x75405 p=t'0'
-;

\x2 name=r9x75405 p=t'-1'
-;

\x2 name=r9x75405 p=t'0'
-;

\x2 name=r9x75405 p=t'3000000000'
-;

\x2 name=r9x75405 rows=2 p=t'7'
-;

\x2 name=r9x75405 rf=1,0 p=t'49'
-;

\x2 name=r9x75405 rf=1 p=b00000031
-;

\x2 name=r9x75405 rf=1,0 p=null
-;

\x2 name=r9x75405 rf=0 p=t'0'
-;

\s2 CREATE INDEX r9s_v75405 ON r9s (v);

\s2 DROP INDEX r9s_v75405;

\x2 name=r9x75405 rf=0 p=t'-1'
-;

\x2 name=r9x75405 rows=1 p=b00000031
-;

\x2 name=r9x75405 rf=0 p=b00000000b2d05e00
-;

\x2 name=r9x75405 p=b00000000
-;

\x2 name=r9x75405 rf=1 p=b00000007
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x75405';

\s2 DEALLOCATE r9x75405;

\s2 RESET plan_cache_mode;

SELECT sum(v) OVER (GROUPS 1 PRECEDING) FROM r8a;

CREATE FUNCTION r8f53726(lim int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE c SCROLL CURSOR (m int) FOR SELECT id FROM r8a WHERE id <= m ORDER BY id; v int; r text := '';
BEGIN
  OPEN c(lim);
  LOOP FETCH c INTO v; EXIT WHEN NOT FOUND; r := r || v || ','; END LOOP;
  FETCH NEXT FROM c INTO v;
  r := r || ' NEXT=' || coalesce(v::text, '-') || ' ' || FOUND;
  MOVE BACKWARD 2 FROM c;
  FETCH c INTO v;
  r := r || ' back=' || coalesce(v::text, '-');
  CLOSE c;
  RETURN r;
END $$;

SELECT r8f53726(4);

SELECT r8f53726(0);

SELECT r8f53726(14);

DROP FUNCTION r8f53726;

\s2 SET plan_cache_mode = force_custom_plan;

\x2 name=r9x73308 types=23,23 describe p=b00000002 p=t'99'
SELECT n.id, n.a / $1 FROM r9n n JOIN r9s s ON s.id = n.id WHERE s.g = $2 ORDER BY n.id LIMIT 3;

\x2 name=r9x73308 rf=1 p=t'0' p=t'1'
-;

\x2 name=r9x73308 rf=1 p=b00000001 p=t'2'
-;

\x2 name=r9x73308 rows=1 p=t'7' p=b00000005
-;

\x2 name=r9x73308 p=b00000007 p=t'2'
-;

\x2 name=r9x73308 rf=1 p=t'7' p=t'1'
-;

\x2 name=r9x73308 rf=1 p=b00000002 p=t'1'
-;

\x2 name=r9x73308 rf=1 p=b00000007 p=t'1'
-;

\x2 name=r9x73308 p=b00000001 p=b00000003
-;

\x2 name=r9x73308 p=t'-1' p=t'1'
-;

\x2 name=r9x73308 rf=1 p=t'-1' p=t'99'
-;

\x2 name=r9x73308 rf=1,0 p=t'2' p=t'99'
-;

\x2 name=r9x73308 rf=1 p=bffffffff p=t'3'
-;

\x2 name=r9x73308 p=t'1' p=b00000002
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x73308';

\s2 DEALLOCATE r9x73308;

\s2 RESET plan_cache_mode;

\s2 BEGIN;

\s2 UPDATE r7k SET v = coalesce(v, 0) + 10, grp = grp + 0 WHERE id = 4;

\s3 BEGIN ISOLATION LEVEL REPEATABLE READ;

\s3 SELECT count(*) FROM r7k;

\s3& SELECT id, v, grp FROM r7k WHERE id = 4 AND grp = 1 FOR UPDATE;

\s2 COMMIT;

\s3.;

\s3 COMMIT;

\s2 ROLLBACK;

\s3 ROLLBACK;

EXPLAIN (COSTS OFF) SELECT tc, count(*), min(ti), max(t) FROM r9n GROUP BY tc ORDER BY tc LIMIT 39;

\x3 name=r9x15626 types=25 describe rf=1 p=t'{2999,1}'
SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;

\x3 name=r9x15626 rf=1 p=b7b312c322c337d
-;

\x3 name=r9x15626 rf=1,0 p=t'{2999,1}'
-;

\x3 name=r9x15626 p=b7b312c322c337d
-;

\x3 name=r9x15626 p=b7b35302c3130302c4e554c4c7d
-;

\x3 name=r9x15626 rf=1,0 p=t'{}'
-;

\x3 name=r9x15626 rf=0 p=t'{}'
-;

\x3 name=r9x15626 rf=1 p=t'{50,100,NULL}'
-;

\x3 name=r9x15626 p=t'{2999,1}'
-;

\x3 name=r9x15626 rows=2 p=b7b35302c3130302c4e554c4c7d
-;

\x3 name=r9x15626 rf=0 p=t'{50,100,NULL}'
-;

\x3 name=r9x15626 rows=2 p=t'{}'
-;

\s3 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x15626';

\s3 DEALLOCATE r9x15626;

\s3 RESET plan_cache_mode;

BEGIN;

DECLARE r9cur SCROLL CURSOR FOR SELECT id, v FROM r9s WHERE id BETWEEN 2254 AND 2264 ORDER BY id DESC;

FETCH BACKWARD 2 FROM r9cur;

FETCH ABSOLUTE 2 FROM r9cur;

MOVE FORWARD ALL FROM r9cur;

CLOSE r9cur;

COMMIT;

\s3 SET ROLE r7_alice;

\s3 UPDATE r7acct SET amount = amount + 1 RETURNING id, amount;

\s3 SELECT current_user, session_user, current_role;

\s3 SELECT count(*) FROM r7k;

\s3 RESET ROLE;

CREATE FUNCTION r9f45017(int, int) RETURNS bigint LANGUAGE plpgsql STABLE AS $f$BEGIN RETURN (SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2); END$f$;

DROP FUNCTION r9f45017;

PREPARE r9p76040 (int, int) AS SELECT id, x FROM r9s WHERE id BETWEEN $1 AND $1 + $2 ORDER BY id DESC LIMIT 20;

EXECUTE r9p76040(777, 0);

EXPLAIN (COSTS OFF) EXECUTE r9p76040(2999, 99);

EXECUTE r9p76040(2999, 99);

EXECUTE r9p76040(2999, 99);

EXECUTE r9p76040(0, 1);

EXPLAIN (COSTS OFF) EXECUTE r9p76040(50, 99);

EXECUTE r9p76040(50, 99);

ANALYZE r9s;

EXPLAIN (COSTS OFF) EXECUTE r9p76040(2147483647, 99);

EXECUTE r9p76040(2147483647, 99);

EXECUTE r9p76040(-5, 5);

EXECUTE r9p76040(1, 1);

EXECUTE r9p76040(100, -1);

EXPLAIN (COSTS OFF) EXECUTE r9p76040(100, 99);

EXECUTE r9p76040(100, 99);

DISCARD PLANS;

EXECUTE r9p76040(1499, 99);

EXECUTE r9p76040(0, 5);

EXPLAIN (COSTS OFF) EXECUTE r9p76040(50, 1);

EXECUTE r9p76040(50, 1);

EXECUTE r9p76040(777, 1);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p76040';

DEALLOCATE r9p76040;

RESET plan_cache_mode;

CREATE TABLE r9a25130 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a25130 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3906, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3906) i;

SET default_statistics_target = 1000;

ALTER TABLE r9a25130 ALTER COLUMN s SET (n_distinct = -1);

ANALYZE (SKIP_LOCKED) r9a25130;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a25130 WHERE s = 'v7'$$);

-- statement
SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a25130$$);
