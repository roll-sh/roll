-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: QUERY PLAN:25
-- engine: HashAggregate  (cost=42.11..42.19 rows=8 width=12)
-- engine:   Group Key: g
-- engine:   ->  Seq Scan on r9a44358  (cost=0.00..35.07 rows=1407 width=4)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: HashAggregate  (cost=42.10..42.18 rows=8 width=12)
-- reference:   Group Key: g
-- reference:   ->  Seq Scan on r9a44358  (cost=0.00..35.07 rows=1407 width=4)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int8, b int8, c date, d timestamptz, e int4, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

CREATE INDEX t0_i722 ON t0 (e);

CREATE INDEX t0_i661 ON t0 (id);

CREATE INDEX t0_i1 ON t0 ((e + 1));

CREATE TABLE t1 (id int4 PRIMARY KEY, a timestamptz, b numeric(10,2), c int2);

INSERT INTO t1 (id, a, b, c) VALUES (1, '2024-01-02 00:00:00+00', 3.75, 3), (3, '2024-01-05 12:00:00+00', 0.25, 2), (19, '2024-01-05 06:00:00+00', 10.00, 6), (27, '2024-01-08 00:00:00+00', 12.25, 3), (29, '2024-01-02 00:00:00+00', 4.00, 3), (40, '2024-01-06 00:00:00+00', 7.25, 3), (11, '2024-01-07 12:00:00+00', NULL, 3), (30, NULL, 6.75, 5), (21, '2024-01-04 06:00:00+00', 2.05, 6), (8, '2024-01-08 23:00:00+00', NULL, 5), (28, '2024-01-04 06:00:00+00', 11.00, 2), (15, '2024-01-05 23:00:00+00', 1.75, 4);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER DELETE ON t1 REFERENCING OLD TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('Z', -1)::ct3, '{}', NULL), (2, ROW('é', 1)::ct3, '{}', 0), (3, ROW('abc', -1)::ct3, ARRAY[ROW('Ä', NULL)::ct3, ROW('ß', 0)::ct3], NULL), (4, ROW('B', 2)::ct3, NULL, 4), (5, ROW('aa', -1)::ct3, ARRAY[ROW(' x', 0)::ct3, ROW('b', -1)::ct3], NULL), (6, ROW('abc', 0)::ct3, ARRAY[ROW('abc', 3)::ct3, ROW('bar', 3)::ct3], NULL), (7, ROW(NULL, 3)::ct3, ARRAY[ROW(NULL, 2)::ct3, ROW('é', 3)::ct3], 0), (8, ROW('', 1)::ct3, ARRAY[ROW('a%', 3)::ct3, ROW(NULL, 3)::ct3], NULL), (9, ROW('x ', 2)::ct3, '{}', 2), (10, NULL, ARRAY[ROW('B', 1)::ct3, ROW('aa', 2)::ct3], 2), (11, ROW('ba', 1)::ct3, '{}', 3), (12, NULL, ARRAY[ROW('a%', 1)::ct3, ROW('Ä', NULL)::ct3], 3), (13, ROW('z', 3)::ct3, ARRAY[ROW('ba', 1)::ct3, ROW('10', 3)::ct3], NULL);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE VIEW v0 AS SELECT NULL::int2 AS c0 FROM t0 t1 WHERE (t1.e & (t1.id)::text::int4) IN (WITH w2 AS NOT MATERIALIZED (SELECT (array_agg(t4.g ORDER BY t4.g NULLS FIRST) FILTER (WHERE t1.d >= t3.d))[1:2] AS c0 FROM (t0 t3 JOIN t0 t4 ON t3.g = t4.g AND t3.id >= t4.id) WHERE COALESCE(NULLIF(t3.e, t1.g) < abs(t3.b), ARRAY[t3.e, t3.e] && '{1,2}'::int4[]) GROUP BY t3.e), w5 AS NOT MATERIALIZED (SELECT (count(*))::int AS c0 FROM t0 t6 WHERE false GROUP BY t6.c, t6.a HAVING starts_with('foo', 'a%')) SELECT (sign((t7.g * t1.id))::int % 2) AS c0 FROM (t0 t7 FULL JOIN t1 t8 ON t7.d = t8.a));

CREATE VIEW v1 AS SELECT (SELECT min('9') FILTER (WHERE t1.a IS NOT NULL) FROM ONLY v0 t5) AS c0, '{5}'::int4[] AS c1, '1 mon -1 day'::interval AS c2 FROM (t1 t1 LEFT JOIN (SELECT NULL::text[] AS c0 FROM t0 t3) s2 ON (SELECT bool_and(false) FROM v0 t4));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', 'infinity', '2025-03-09 02:30:00', '2000-03-01', '36 hours', '0', 'Infinity', 10), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', 'infinity', '2000-03-01', '-1 mon 2 days', '0.5', '1024', NULL), (3, 'null', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '4713-01-01 BC', '36 hours', NULL, '0.125', 12), (4, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', '2000-01-01 00:00:00', '2025-03-09', '-12:00', '123.45', NULL, 10), (5, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-11-02 01:30:00-05', '2025-12-31 23:59:59.999999', '2024-02-29', '36 hours', '123.45', 'NaN', NULL), (6, '"str"', '"str"', '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '2024-02-29', '-1 mon 2 days', '2.5', 'NaN', 12), (7, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', 'infinity', '-infinity', '-1 mon 2 days', NULL, '1024', -1);

INSERT INTO tp4 VALUES (7, 'infinity', 'b', 'NaN'), (20, '2025-03-09', 'b', NULL), (10, '1999-12-31', 'b', NULL), (21, '2024-02-29', NULL, '123.45'), (17, 'infinity', 'd', 'NaN');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, NULL, 'e', '-99999999999999999999999999999999999.999999', '2147483647 days', '[5,6)', NULL, 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(-2, '-a')::r5ct, ROW(2, 'I')::r5ct]), (2, 0, 'ä', NULL, '-1 mon 30 days', '[1,5)', '{}', '', NULL), (3, NULL, 'å', '7', '0', '(,)', '{(,0],[10,)}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(3, NULL)::r5ct, ROW(-1, 'I')::r5ct, ROW(4, 'c')::r5ct]), (4, -3, NULL, '1e-130', '2147483647 mons', '[3,)', '{}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(-1, 'å')::r5ct, ROW(2, 'A')::r5ct]), (5, 11, 'å', NULL, '178000000 years', '[0,10)', '{[1,3),[5,7)}', '', ARRAY[ROW(4, 'A')::r5ct, ROW(3, NULL)::r5ct]), (6, 16, '2', '0', '1 mon', '[1,5)', '{}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(-2, '-a')::r5ct]), (7, 6, '10', '-Infinity', '0', '[1,5)', '{}', NULL, ARRAY[ROW(5, 'A')::r5ct]), (8, NULL, 'o', NULL, NULL, '[5,6)', '{(,0],[10,)}', '', ARRAY[ROW(NULL, 'E')::r5ct, ROW(NULL, 'Z')::r5ct]), (9, -1, 'ß', 'NaN', '-2147483648 days', '[0,10)', '{[0,100)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(1, 'I')::r5ct]), (10, 19, 'å', '123456789012345678901234567890.123456789', '-178000000 years', '(1,5]', '{[2,4)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(5, 'B')::r5ct]), (11, -1, '2', '-Infinity', '-2147483648 days', '[0,10)', '{empty}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(0, 'ö')::r5ct, ROW(0, '-a')::r5ct]), (12, -1, 'b', '0', '30 days', '(,2]', '{(,0],[10,)}', 'PostgreSQL full text search: lexemes & weights', '{}');

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 26;

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

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (-3, 'a', '2024-02-29'), (-1, 'b', '2025-01-01'), (18, 'c', '2025-01-01'), (-5, 'd', NULL), (-5, 'a', '2025-01-01'), (10, 'b', '2025-01-01'), (14, 'c', '2024-02-29'), (5, 'd', 'infinity'), (19, 'a', '2024-02-29') ON CONFLICT DO NOTHING;

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'zoë1', 1, NULL, '{}'), (2, 'u2@x.org', '2', 2, '{a}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'Éva3', 3, '{a}', NULL), (4, 'u4@x.org', 'dee4', 4, '{a,b}', '[1, {"a": null}]'), (5, 'u5@x.org', 'Éva5', 5, '{}', '[1, {"a": null}]'), (6, 'u6@x.org', 'bob6', 0, '{}', '{}'), (7, 'u7@x.org', 'ann7', 7, '{a}', '{"seats": 1.50}'), (8, 'u8@x.org', 'zoë8', 8, '{a,b}', '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (8, 0, 'new', '2025-02-19', '{"items": []}'), (8, -1.50, 'paid', '2025-02-14', '{"items": []}'), (6, 0, 'void', '2025-02-27', '{"gift": true}'), (7, 12345678.90, 'void', '2025-02-20', NULL), (4, 0, 'new', '2025-02-12', '{"items": [{"sku": "a", "qty": 2}]}'), (6, 9.99, 'void', '2025-02-22', NULL), (6, 0, 'void', '2025-02-15', NULL), (7, 0, 'void', '2025-02-14', NULL), (8, 0, 'paid', '2025-02-15', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 0, 'new', '2025-02-23', NULL), (8, 9.99, 'new', '2025-02-16', NULL), (4, -1.50, 'new', '2025-02-03', '{"items": [{"sku": "a", "qty": 2}]}'), (8, -1.50, 'void', '2025-02-21', '{"items": [{"sku": "a", "qty": 2}]}'), (4, -1.50, 'void', '2025-02-24', NULL), (6, NULL, 'void', '2025-02-03', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 128, 'NOTE 3', 0, 'NaN', '2025-03-09 07:00:00+00'), (2, 3, 161, 'note 2', 0.5, 0.5, '2025-03-09 06:59:59+00'), (3, 4, 91, 'n%5', 1000, 0.5, NULL), (4, 7, 35, '', 3.333, 0.5, '2025-11-02 05:30:00+00'), (5, 1, NULL, 'note 3', 3.333, 1e300, '2025-03-09 06:59:59+00'), (6, 1, 41, 'note 2', 3.333, 1e300, '2025-03-09 06:59:59+00'), (7, 1, 39, 'note 3', 0.5, -0, '2025-11-02 05:30:00+00'), (8, 6, 53, 'é', 0, 2.25, NULL), (9, 1, 45, 'note 2', NULL, 'Infinity', '2024-02-29 12:00:00+00'), (10, 1, 13, '', 3.333, NULL, '2025-03-09 07:00:00+00'), (11, 1, 87, 'note 2', 0, 'Infinity', '2025-03-09 06:59:59+00'), (12, 1, 200, 'note 1', -12.125, 2.25, '2025-03-09 07:00:00+00'), (13, 1, 137, 'Note 3', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (14, 7, -17, 'note 3', NULL, 0.5, '2025-03-09 06:59:59+00'), (15, 4, 189, 'é', 0.5, NULL, '2024-02-29 12:00:00+00'), (16, 1, 111, 'Note 3', -12.125, 2.25, '2024-02-29 12:00:00+00'), (17, 1, 169, 'NOTE 3', NULL, 0.5, '2025-03-09 06:59:59+00'), (18, 4, 171, 'Note 3', 1000, 'NaN', NULL), (19, 1, 44, 'note 2', NULL, 'NaN', '2025-03-09 06:59:59+00'), (20, 3, 10, 'n%5', 3.333, NULL, '2025-11-02 05:30:00+00'), (21, 1, 107, 'Note 3', -12.125, 1e300, '2025-11-02 05:30:00+00'), (22, 1, NULL, 'note_4', NULL, 'NaN', '2024-02-29 12:00:00+00'), (23, 1, 155, 'n%5', -12.125, NULL, '2025-03-09 06:59:59+00'), (24, 8, 183, '', 0, 'Infinity', '2024-02-29 12:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

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

INSERT INTO r7t VALUES (1, 'ä b', 'ä b'), (2, 'ss', 'ss'), (3, 'ß', 'ß'), (4, 'ä', 'ä'), (5, '10', '10'), (6, 'Æ', 'Æ'), (7, 'é', 'é'), (8, 'ss', 'ss'), (9, '10', '10'), (10, 'e', 'e');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, 3, 0, 'ab', '2025-01-01'::timestamp + interval '20 hours'), (2, 1, -3, 0, 'a', '2025-01-01'::timestamp + interval '165 hours'), (3, 1, 5, 1.50, 'z', '2025-01-01'::timestamp + interval '153 hours'), (4, 4, 19, 1.50, 'ab', '2025-01-01'::timestamp + interval '22 hours'), (5, 4, 12, 3.75, 'a', '2025-01-01'::timestamp + interval '132 hours'), (6, 2, NULL, -2.25, 'B', '2025-01-01'::timestamp + interval '38 hours'), (7, 3, NULL, 0, 'a', '2025-01-01'::timestamp + interval '26 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 3, 3), (1, 2, 9), (2, 6, 7), (3, 1, 5), (4, 4, 6), (5, 5, 8), (5, 3, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[6,9)', '{(,6)}', 'empty', '(1.25,1.5)'), (2, '(1,6]', '{(,1)}', '(2024-12-31,2025-01-05]', '[0,0]'), (3, 'empty', '{[-2,1), [3, 5)}', '(2024-12-31,2025-01-05]', NULL), (4, 'empty', '{(,-2)}', '(2024-12-31,2025-01-05]', '[0,0]'), (5, NULL, '{(,9)}', '[2025-03-01,infinity)', NULL), (6, '[8,)', NULL, '[2025-01-01,2025-02-01)', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'dog-eat-dog world'), (2, 'running rats'), (3, 'brown''s fox''s den'), (4, 'a rat ran');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2570) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1680) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3166) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

\s2 BEGIN;

\s2 UPDATE r9s SET v = coalesce(v, 0) + 1000 WHERE id BETWEEN 564 AND 567 RETURNING id, v;

\s2 INSERT INTO r9s VALUES (10564, 1, 7, 'new', 1.5);

\s2 COMMIT;

VACUUM r9s;

EXPLAIN (COSTS OFF) SELECT id, t FROM r9s WHERE t >= 'a' AND t < 'b' ORDER BY t , id LIMIT 29;

SET plan_cache_mode = force_custom_plan;

PREPARE r9p13433 (text) AS SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;

EXECUTE r9p13433('{2999,1}');

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{1,2,3}');

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{50,100,NULL}');

CREATE INDEX r9s_v13433 ON r9s (v);

DROP INDEX r9s_v13433;

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{1,2,3}');

EXECUTE r9p13433('{}');

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{50,100,NULL}');

EXECUTE r9p13433('{2999,1}');

EXECUTE r9p13433('{50,100,NULL}');

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p13433';

DEALLOCATE r9p13433;

RESET plan_cache_mode;

CREATE TABLE r8c55655 (id int NOT NULL, a text, b int DEFAULT 42, c date);

COPY r8c55655 (id, b, a, c) FROM STDIN (FORMAT csv, HEADER, ON_ERROR ignore);
id,a,x,c
1,multi word,\D
2,,,,extra
\.

COPY r8c55655 TO STDOUT (FORMAT csv, HEADER MATCH);

COPY r8c55655 FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 0);

DROP TABLE r8c55655;

CREATE FUNCTION r8ef57666() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'rewrite % % %', tg_tag, pg_event_trigger_table_rewrite_oid()::regclass, pg_event_trigger_table_rewrite_reason(); END $$;

CREATE EVENT TRIGGER r8ef57666 ON table_rewrite WHEN TAG IN ('CREATE TABLE', 'DROP TABLE', 'ALTER TABLE', 'COMMENT') EXECUTE FUNCTION r8ef57666();

CREATE TABLE r8e57666 (a int PRIMARY KEY, b text);

CREATE INDEX r8e57666_b ON r8e57666 (b);

ALTER TABLE r8e57666 ALTER COLUMN a TYPE bigint;

ALTER TABLE r8e57666 ADD COLUMN d timestamptz DEFAULT clock_timestamp();

CREATE VIEW r8e57666_v AS SELECT a FROM r8e57666;

ALTER TABLE r8e57666 DROP COLUMN b;

DROP TABLE r8e57666 CASCADE;

SELECT evtname, evtevent, evtenabled, evttags FROM pg_event_trigger WHERE evtname = 'r8ef57666';

DROP EVENT TRIGGER r8ef57666;

DROP FUNCTION r8ef57666;

CREATE TABLE r9i40951 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i40951_1 PARTITION OF r9i40951 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i40951_2 PARTITION OF r9i40951 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i40951 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 299) i;

ANALYZE r9i40951;

CREATE INDEX r9i40951_x0 ON r9i40951 USING btree (a);

SET enable_seqscan = on;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i40951 WHERE abs(a - 40) < 3;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i40951;

CREATE TABLE r9a18082 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a18082 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 735, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 735) i;

ALTER TABLE r9a18082 ALTER COLUMN g SET STATISTICS 100;

ALTER TABLE r9a18082 ALTER COLUMN u SET (n_distinct = 0);

UPDATE r9a18082 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a18082 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a18082 WHERE g = 2 AND s = 'common'$$);

SELECT * FROM r9est($$SELECT * FROM r9a18082 WHERE nul > 100$$);

EXPLAIN SELECT * FROM r9a18082 WHERE nul > 100;

DROP TABLE r9a18082;

EXPLAIN (COSTS OFF) SELECT id, v FROM r9s WHERE id BETWEEN 991 AND 1491 ORDER BY id DESC LIMIT 28;

\s2 LISTEN r8ch88911;

NOTIFY r8ch88911, 'from s1';

\s2 SELECT 1;

\s3 BEGIN;

\s3 NOTIFY r8ch88911, 'a';

\s3 NOTIFY r8ch88911, 'a';

\s3 SELECT pg_notify('r8ch88911', '');

\s3 COMMIT;

\s2 SELECT 2;

\s3 SELECT pg_listening_channels();

\s2 SELECT pg_listening_channels(), pg_notification_queue_usage() < 0.5;

\s2 UNLISTEN r8ch88911;

NOTIFY r8ch88911, 'after unlisten';

\s2 SELECT 3;

SELECT pg_notify(NULL, 'x');

\s3 UNLISTEN *;

\s2 UNLISTEN *;

INSERT INTO r7g (id, a) OVERRIDING USER VALUE VALUES (1002, 1) RETURNING *;

EXECUTE r5qa(1);

SELECT name, parameter_types::text, result_types::text, from_sql FROM pg_prepared_statements ORDER BY name;

PREPARE r9p82753 (int) AS SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

EXECUTE r9p82753(49);

EXPLAIN (COSTS OFF) EXECUTE r9p82753(NULL);

EXECUTE r9p82753(NULL);

EXECUTE r9p82753(NULL);

EXECUTE r9p82753(-50);

EXECUTE r9p82753(0);

SET plan_cache_mode = force_custom_plan;

EXECUTE r9p82753(-50);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p82753';

DEALLOCATE r9p82753;

RESET plan_cache_mode;

SET TimeZone = 'UTC';

SET extra_float_digits = 1;

CREATE FUNCTION r9f52798(int, int) RETURNS bigint LANGUAGE sql STABLE AS $f$SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2$f$;

DROP FUNCTION r9f52798;

EXPLAIN (COSTS OFF) SELECT id, a * c, b * c, b + b, c + c, -a, abs(a), a << 1 FROM r9n WHERE id % 2 = 1 ORDER BY id LIMIT 32;

CREATE TABLE r9a44358 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a44358 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1407, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1407) i;

SET default_statistics_target = 25;

ANALYZE (SKIP_LOCKED) r9a44358;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a44358 WHERE g + 1 = 2$$);

SELECT * FROM r9est($$SELECT g, count(*) FROM r9a44358 GROUP BY g$$);

-- statement
EXPLAIN SELECT g, count(*) FROM r9a44358 GROUP BY g;
