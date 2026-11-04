-- class: wrong_rows
-- ordered: false
-- note: 9 vs 6 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: ((r9i95963.p <-> '(3,3)'::point)), r9i95963.id
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i95963_1.p <-> '(3,3)'::point)), r9i95963_1.id
-- engine:               ->  Seq Scan on r9i95963_1
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i95963_2.p <-> '(3,3)'::point)), r9i95963_2.id
-- engine:               ->  Seq Scan on r9i95963_2
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: ((r9i95963.p <-> '(3,3)'::point)), r9i95963.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i95963_1
-- reference:               ->  Seq Scan on r9i95963_2
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a date, b numeric(10,2), c int4[], d numeric(10,2)) PARTITION BY RANGE (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO ('2024-02-01');

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b, c, d) VALUES (37, NULL, 9.25, '{1,1,2}', 11.50), (6, '2024-03-02', 1.50, NULL, -1.05), (21, '2024-01-11', -2.00, '{2}', NULL), (24, NULL, 2.75, NULL, 6.05), (1, '2024-02-26', 3.00, '{NULL,3}', -3.00);

CREATE UNIQUE INDEX t0_i507 ON t0 (id) NULLS NOT DISTINCT;

CREATE INDEX t0_i408 ON t0 (b);

CREATE TABLE t1 (id int4 PRIMARY KEY, a text, b timestamp);

INSERT INTO t1 (id, a, b) VALUES (22, 'b', '2024-01-04 06:30:00'), (24, 'abc', '2024-01-05 12:30:00'), (11, '10', '2024-01-04 23:00:00'), (46, 'a%', NULL), (35, 'ba', '2024-01-01 00:30:00'), (17, 'Ä', '2024-01-07 06:30:00'), (8, 'Z', NULL), (28, 'a', NULL), (55, 'a_b', '2024-01-04 23:30:00'), (3, 'A', '2024-01-07 23:30:00'), (21, 'é', '2024-01-03 23:00:00'), (7, NULL, NULL), (31, 'Ab', NULL), (27, NULL, '2024-01-08 12:30:00'), (43, NULL, '2024-01-03 06:30:00'), (18, 'x ', '2024-01-02 23:00:00'), (39, NULL, '2024-01-09 00:30:00'), (26, NULL, '2024-01-07 12:00:00'), (14, NULL, '2024-01-08 06:00:00'), (10, 'z', NULL), (16, ' x', NULL), (41, NULL, '2024-01-06 00:00:00'), (38, 'abc', NULL), (1, 'a', NULL), (36, 'b', '2024-01-08 23:00:00'), (12, 'é', '2024-01-07 23:30:00'), (42, NULL, NULL), (33, 'Z', '2024-01-08 00:00:00'), (29, NULL, '2024-01-02 23:30:00'), (23, 'b', NULL);

CREATE INDEX t1_i460 ON t1 (id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t2 (id int4, a text, b numeric(10,2), c numeric(10,2) DEFAULT 2.75);

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2 (id, a, b, c) VALUES (21, 'a', 3.00, NULL), (5, NULL, 9.50, NULL), (39, NULL, 10.25, 10.05), (20, NULL, 12.50, 12.75), (17, NULL, 6.05, NULL), (27, NULL, NULL, NULL), (53, NULL, 4.25, NULL), (4, 'a_b', 9.25, 10.00), (56, NULL, NULL, 6.50), (11, NULL, -1.00, NULL), (12, NULL, NULL, 3.00), (31, 'a,b', 0.75, -1.00), (48, NULL, 1.05, 2.05), (19, NULL, NULL, 0.05), (40, NULL, NULL, NULL), (54, 'b', NULL, 5.25), (7, 'A', NULL, 3.00), (34, '9', 9.75, NULL), (24, 'b', 12.00, 0.25), (38, NULL, 3.50, 10.00), (51, 'z', NULL, NULL), (18, NULL, NULL, NULL), (8, 'aa', -3.25, 8.25), (59, 'Ä', NULL, 1.00), (42, NULL, -2.25, NULL), (35, NULL, 5.50, 1.00), (15, 'aa', 10.05, NULL), (57, 'a%', NULL, -1.05), (30, 'z', NULL, -1.05), (26, NULL, 2.75, 0.00);

CREATE INDEX t2_i503 ON t2 (lower(a));

CREATE UNIQUE INDEX t2_i996 ON t2 (id) NULLS NOT DISTINCT;

CREATE INDEX t2_i693 ON t2 (b) WHERE b IS NOT NULL;

CREATE TABLE t3 (a timestamptz, b numeric(10,2) DEFAULT 4.05, c int4, d d3text, e text) PARTITION BY RANGE (c);

CREATE TABLE t3_p0 PARTITION OF t3 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t3_p1 PARTITION OF t3 FOR VALUES FROM (3) TO (6);

CREATE TABLE t3_pd PARTITION OF t3 DEFAULT;

INSERT INTO t3 (a, b, c, d, e) VALUES (NULL, NULL, NULL, NULL, 'ba'), (NULL, NULL, NULL, NULL, 'Ä'), (NULL, 3.05, 0, NULL, 'aa'), ('2024-01-08 06:00:00+00', NULL, NULL, NULL, NULL), ('2024-01-08 06:00:00+00', 9.00, -2, NULL, NULL), (NULL, NULL, NULL, 'a%', 'ß'), ('2024-01-05 00:00:00+00', 7.25, NULL, 'abc', NULL), (NULL, -3.25, 4, NULL, NULL), ('2024-01-08 23:00:00+00', NULL, 8, NULL, 'ab'), (NULL, NULL, NULL, 'a%', NULL), (NULL, NULL, NULL, '', NULL), (NULL, 12.00, -1, 'ß', NULL), ('2024-01-01 12:00:00+00', NULL, 1, NULL, NULL), (NULL, 1.05, NULL, '', NULL), ('2024-01-04 12:00:00+00', NULL, NULL, NULL, '10'), (NULL, 12.00, NULL, NULL, NULL), ('2024-01-09 06:00:00+00', NULL, NULL, NULL, NULL), (NULL, 4.25, NULL, 'ß', NULL), ('2024-01-04 06:00:00+00', NULL, -2, 'Ab', 'z'), (NULL, 7.00, 1, 'a%', 'Ä'), (NULL, NULL, NULL, NULL, 'a%'), (NULL, 10.00, NULL, NULL, 'ba'), (NULL, 12.00, 7, NULL, NULL), (NULL, NULL, NULL, NULL, NULL), (NULL, NULL, NULL, 'ß', 'Ä'), (NULL, 7.00, NULL, NULL, 'foo'), (NULL, NULL, NULL, NULL, 'Z'), (NULL, 7.75, 0, NULL, '9'), ('2024-01-08 06:00:00+00', NULL, 2, NULL, NULL), (NULL, NULL, NULL, '10', NULL);

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW(NULL, -1)::ct3, '{}', 3), (2, ROW(' x', 1)::ct3, ARRAY[ROW('é', 1)::ct3, ROW('Ä', -1)::ct3], 2), (3, ROW('9', -1)::ct3, ARRAY[ROW('A', 2)::ct3, ROW(NULL, 0)::ct3], 3), (4, ROW('B', 2)::ct3, ARRAY[ROW('b', 2)::ct3, ROW('ß', 2)::ct3], 4), (5, ROW('é', -1)::ct3, NULL, 2), (6, ROW('ab', NULL)::ct3, '{}', 1), (7, NULL, '{}', 0), (8, ROW('ab', -1)::ct3, ARRAY[ROW('A', NULL)::ct3, ROW('A', 3)::ct3], 0), (9, ROW('abc', NULL)::ct3, NULL, 0), (10, ROW('Ä', -1)::ct3, '{}', NULL), (11, ROW('Z', NULL)::ct3, '{}', 1), (12, ROW('9', NULL)::ct3, NULL, 1);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN substr(q, 0, 1) NOT IN (SELECT (COALESCE('a,b', q) COLLATE "default") FROM t1 t1 , t3 t2 WHERE (SELECT t3.d FROM t3 t3 ORDER BY 1 LIMIT 1) >= md5(t2.d) COLLATE "POSIX" GROUP BY GROUPING SETS ((t2.a), ())) THEN RETURN ascii((q || ' x'));
  ELSE RETURN length(q);
  END CASE;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN NULL::bool THEN GREATEST(8, (- 1)) ELSE extract(day from ('2024-09-07 22:00:00'::timestamp)::date)::int END $$;

CREATE VIEW v0 AS SELECT DISTINCT 'ba' AS c0, NULL::int4 AS c1, ('{5}'::int4[] || ((2 / 1) * ('{1,2}'::int4[])[1])) AS c2, ('2025-11-28 05:00:00'::timestamp)::date AS c3 FROM (VALUES ('2000-12-22'::date), ('1999-12-25'::date), (NULL)) s1(v0);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2025-11-02 01:30:00', 'infinity', '23:59:59.999999', '0', '2', -2), (2, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-10-26 00:30:00+00', '2025-03-09 02:30:00', 'infinity', '-infinity', '0.5', '1024', 2), (3, '"str"', '"str"', '0044-03-15 12:00:00+00 BC', '2025-11-02 01:30:00', '-infinity', '0', 'NaN', '-0', 11), (4, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2024-02-29 00:00:00', '-infinity', '23:59:59.999999', '0.000001', '0.125', 10), (5, NULL, NULL, '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '2025-03-09', NULL, '-7', 'NaN', -2), (6, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '2024-02-29 00:00:00', '2025-03-09', '36 hours', '-0.0', '0.5', 9), (7, NULL, NULL, NULL, '2024-02-29 00:00:00', '2000-03-01', '2 weeks 3 microseconds', '-Infinity', '2', 8), (8, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '0001-01-01 00:00:00 BC', '4713-01-01 BC', '23:59:59.999999', 'Infinity', '0.5', -3), (9, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '1970-01-01 00:00:00.5', '-infinity', 'infinity', NULL, 'Infinity', 12), (10, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', NULL, '2025-12-31 23:59:59.999999', '2000-03-01', '23:59:59.999999', NULL, '0.5', 6), (11, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2000-01-01 00:00:00', 'infinity', '0', 'NaN', '2', -3), (12, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2025-04-06 02:30:00+11', '2025-12-31 23:59:59.999999', NULL, '1 year 2 mons', '0.5', '0.125', 12), (13, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', NULL, NULL, '1 day', '1.23456789012345678901234567890', '0.125', 1);

INSERT INTO tp4 VALUES (10, '2025-03-09', 'a', '-Infinity'), (5, NULL, 'ab', '-7'), (15, '1999-12-31', 'b', '1e30');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5ai", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 15, 'SS', '1e-130', '2147483647 days', 'empty', '{[2,4)}', NULL, '{}'), (2, 15, 'SS', '7', '0', '[-3,0]', '{[2,4)}', 'the and of', ARRAY[ROW(3, 'i')::r5ct]), (3, -3, 'Ab', '1e-130', '720:00:00', '[5,6)', '{(,0],[10,)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL), (4, 1, '2', '0.0000000000000000000000000001', '-1 mon 30 days', '(1,5]', '{(,0],[10,)}', 'running runners ran; the runner runs', ARRAY[ROW(NULL, '-a')::r5ct]), (5, -1, 'ae', 'NaN', '2147483647 days', '[1,5)', '{[0,100)}', NULL, ARRAY[ROW(NULL, NULL)::r5ct]), (6, 2, NULL, '1e100', '2147483647 days', '[3,)', '{(,0],[10,)}', 'the and of', ARRAY[ROW(4, 'h')::r5ct]), (7, 1, 'ch', '7', '1 mon', '[0,10)', '{[0,100)}', '', NULL), (8, NULL, 'ß', '0', '2147483647 days', '(1,5]', '{}', 'the and of', ARRAY[ROW(4, 'b')::r5ct, ROW(3, NULL)::r5ct]), (9, 19, 'ab', '0', '2147483647 mons', '(,)', '{}', 'running runners ran; the runner runs', '{}'), (10, 18, '', '0.0000000000000000000000000001', '2147483647 days', '(,)', '{[1,3),[5,7)}', NULL, '{}');

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_b ON r5a (b COLLATE "de-x-icu");

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (NEW.g1 > 0) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 23 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (-1, 'a', NULL), (-5, 'b', '2024-02-29'), (9, 'c', '2025-01-01'), (19, 'd', '2025-01-01'), (15, 'a', '2025-01-01'), (-1, 'b', '2025-01-01'), (-2, 'c', '2025-01-01'), (17, 'd', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (29, 'x0', NULL), (30, 'x1', NULL), (24, 'x2', NULL), (28, 'x3', NULL), (28, 'x4', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Éva1', 1, '{}', '{"plan": "pro", "seats": 3}'), (2, 'u2@x.org', 'bob2', 2, '{}', NULL), (3, 'u3@x.org', 'dee3', 3, '{}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', 'bob 4', 4, '{a,b}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (4, 12345678.90, 'new', '2025-02-12', '{"items": [{"sku": "a", "qty": 2}]}');

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

INSERT INTO r7k VALUES (1, 1, NULL, 'NOTE 3', -12.125, -0, '2025-03-09 06:59:59+00'), (2, 1, 190, 'Note 3', 0, -0, NULL), (3, 9, 192, 'note 3', NULL, -0, '2025-11-02 05:30:00+00'), (4, 1, NULL, 'NOTE 3', -12.125, 'NaN', '2025-03-09 07:00:00+00'), (5, 1, NULL, 'note 2', -12.125, 1e300, '2025-11-02 05:30:00+00'), (6, 1, 191, 'n%5', 3.333, 1e300, '2025-03-09 06:59:59+00'), (7, 1, 75, 'NOTE 3', 1000, 'NaN', '2025-03-09 07:00:00+00'), (8, 1, 182, '', 3.333, -0, '2025-11-02 05:30:00+00'), (9, 1, 100, 'note 2', NULL, 0.5, '2025-03-09 07:00:00+00'), (10, 1, 61, 'note 2', NULL, 'Infinity', '2025-03-09 06:59:59+00'), (11, 1, -3, 'note 2', -12.125, 2.25, '2025-03-09 07:00:00+00'), (12, 7, -13, '', NULL, 0.5, '2025-11-02 05:30:00+00'), (13, 1, NULL, 'note 3', 0, -0, '2025-11-02 05:30:00+00'), (14, 1, 141, 'note 3', 3.333, 0.5, '2025-03-09 07:00:00+00'), (15, 1, 39, 'NOTE 3', 1000, 1e300, '2024-02-29 12:00:00+00'), (16, 1, 130, 'n%5', NULL, 0.5, '2025-03-09 07:00:00+00'), (17, 1, 103, 'note_4', -12.125, 'Infinity', '2025-03-09 07:00:00+00'), (18, 1, 171, 'NOTE 3', NULL, 1e300, '2025-03-09 07:00:00+00'), (19, 2, 160, '', -12.125, NULL, '2025-03-09 07:00:00+00'), (20, 1, -5, 'n%5', 3.333, 1e300, '2025-03-09 07:00:00+00'), (21, 1, -33, 'note_4', 0.5, 2.25, '2025-03-09 07:00:00+00'), (22, 2, -20, 'note 2', NULL, 'NaN', NULL), (23, 1, 76, 'Note 3', 3.333, 2.25, '2025-11-02 05:30:00+00'), (24, 1, -5, '', -12.125, NULL, '2025-03-09 07:00:00+00'), (25, 1, 95, 'NOTE 3', NULL, 'NaN', '2025-11-02 05:30:00+00');

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

INSERT INTO r7t VALUES (1, 'e', 'e'), (2, 'Æ', 'Æ'), (3, 'a', 'a'), (4, 'ä', 'ä'), (5, 'ae', 'ae'), (6, 'e', 'e'), (7, 'Ä', 'Ä'), (8, 'Ä', 'Ä'), (9, 'e', 'e'), (10, 'A', 'A');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, 15, 0, '', '2025-01-01'::timestamp + interval '58 hours'), (2, 4, NULL, -2.25, NULL, '2025-01-01'::timestamp + interval '105 hours'), (3, 2, 3, 0, 'ab', '2025-01-01'::timestamp + interval '33 hours'), (4, 2, 20, 1.50, '', '2025-01-01'::timestamp + interval '12 hours'), (5, 3, 11, -2.25, 'z', '2025-01-01'::timestamp + interval '207 hours'), (6, 3, 7, 1.50, NULL, '2025-01-01'::timestamp + interval '57 hours'), (7, 3, 1, 1.50, 'ab', '2025-01-01'::timestamp + interval '65 hours'), (8, 4, -1, 3.75, 'b', '2025-01-01'::timestamp + interval '32 hours'), (9, 1, 19, NULL, '', '2025-01-01'::timestamp + interval '32 hours'), (10, 4, 9, 0, NULL, '2025-01-01'::timestamp + interval '70 hours'), (11, 1, 8, 3.75, 'z', '2025-01-01'::timestamp + interval '40 hours'), (12, 3, 13, NULL, 'z', '2025-01-01'::timestamp + interval '201 hours'), (13, 4, 2, 10, 'z', '2025-01-01'::timestamp + interval '46 hours'), (14, 3, 10, 1.50, 'z', '2025-01-01'::timestamp + interval '0 hours'), (15, 3, 15, 1.50, '', '2025-01-01'::timestamp + interval '38 hours'), (16, 4, 6, 1.50, NULL, '2025-01-01'::timestamp + interval '63 hours'), (17, 4, 19, 10, 'z', '2025-01-01'::timestamp + interval '38 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 5, 6), (1, 5, 3), (3, 5, 7), (3, 1, 9), (4, 1, 2), (4, 3, 6), (5, 2, 7), (5, 1, 5), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-2,)', '{[-2,1), [3, 5)}', 'empty', '[0,0]'), (2, 'empty', '{(,10)}', '[2025-01-01,2025-02-01)', '(,0]'), (3, '(-2,1]', NULL, '(2024-12-31,2025-01-05]', NULL), (4, '(-2,3]', '{(,-2)}', '[2025-03-01,infinity)', NULL), (5, '[3,)', '{[3,3), [5, 7)}', NULL, NULL), (6, NULL, NULL, NULL, '[1.5,2.5)'), (7, '[8,)', '{}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (8, '[3,3)', '{[3,3), [5, 7)}', 'empty', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'a rat ran'), (2, 'foxes and dogs'), (3, 'a rat ran'), (4, 'brown''s fox''s den'), (5, 'brown''s fox''s den'), (6, 'foxes and dogs'), (7, 'foxes and dogs'), (8, 'über café naïve');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (5, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3746) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2702) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3535) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

PREPARE r9p27923 (int, int) AS SELECT count(*) FROM r9s WHERE g = $1 AND v > 10 / $2;

EXECUTE r9p27923(99, 7);

EXECUTE r9p27923(1, 7);

EXECUTE r9p27923(5, 2);

EXECUTE r9p27923(2, -1);

EXECUTE r9p27923(2, 1);

EXECUTE r9p27923(3, 0);

EXECUTE r9p27923(5, 7);

EXECUTE r9p27923(1, 7);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p27923';

DEALLOCATE r9p27923;

RESET plan_cache_mode;

CREATE TABLE r9a26915 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a26915 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4389, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4389) i;

CREATE STATISTICS r9a26915_st ON g, (u % 10) FROM r9a26915;

UPDATE r9a26915 SET g = 5, nul = NULL WHERE id % 50 = 0;

VACUUM ANALYZE r9a26915;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a26915 WHERE b$$);

SELECT * FROM r9est($$SELECT * FROM r9a26915 WHERE g = 1 AND u % 10 = 1$$);

SELECT * FROM r9est($$SELECT * FROM r9a26915 WHERE g = 1$$);

DROP TABLE r9a26915;

CREATE FUNCTION r8f17209(m int) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'r8cur17209';
BEGIN OPEN c FOR EXECUTE format('SELECT id, g FROM r8a WHERE id <= %s ORDER BY id', m); RETURN c; END $$;

BEGIN;

SELECT r8f17209(3) IS NOT NULL;

FETCH 2 FROM r8cur17209;

FETCH ALL FROM r8cur17209;

SELECT name LIKE '<unnamed portal%' AS unnamed, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY name;

COMMIT;

DROP FUNCTION r8f17209;

ALTER TABLE r5a ADD CONSTRAINT r5f FOREIGN KEY (a) REFERENCES r5b (k) NOT VALID;

CREATE TABLE r9i55349 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i55349 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 135) i;

ANALYZE r9i55349;

CREATE INDEX ON r9i55349 USING btree (a) WHERE a > 50;

INSERT INTO r9i55349 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(136, 273) i;

UPDATE r9i55349 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i55349 WHERE id % 11 = 0;

SET enable_seqscan = off;

SET enable_bitmapscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i55349 WHERE t = 'b4';

EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i55349 ORDER BY p <-> point '(3,3)', id LIMIT 12;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i55349;

CREATE TABLE r8t92590 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t92590 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf92590() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t92590', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf92590_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t92590', r);
  RETURN NULL;
END $$;

CREATE TRIGGER i_td AFTER DELETE ON r8t92590 REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r8tf92590_tt();

CREATE TRIGGER e_tr BEFORE TRUNCATE ON r8t92590 FOR EACH STATEMENT EXECUTE FUNCTION r8tf92590();

CREATE TRIGGER b_air AFTER INSERT OR DELETE ON r8t92590 FOR EACH ROW EXECUTE FUNCTION r8tf92590();

CREATE TRIGGER i_td AFTER DELETE ON r8t92590 REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r8tf92590_tt();

INSERT INTO r8t92590 VALUES (10, 1, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't92590';

DELETE FROM r8t92590 WHERE id = 3 RETURNING *;

DELETE FROM r8log WHERE tg = 't92590';

INSERT INTO r8t92590 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't92590';

BEGIN;

UPDATE r8t92590 SET s = 'deferred' WHERE id = 2;

ROLLBACK;

COMMIT;

DELETE FROM r8log WHERE tg = 't92590';

DROP TABLE r8t92590;

DROP FUNCTION r8tf92590, r8tf92590_tt;

CREATE TABLE r9i95963 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i95963_1 PARTITION OF r9i95963 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i95963_2 PARTITION OF r9i95963 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i95963 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3713) i;

CREATE INDEX ON r9i95963 USING hash (t);

CREATE INDEX r9i95963_x1 ON r9i95963 USING gist (r);

ALTER TABLE r9i95963 ADD CONSTRAINT r9i95963_ex EXCLUDE USING gist (p WITH ~=);

INSERT INTO r9i95963 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(3714, 3759) i;

UPDATE r9i95963 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i95963 WHERE id % 11 = 0;

SET enable_seqscan = on;

SET enable_indexonlyscan = off;

-- statement
EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i95963 ORDER BY p <-> point '(3,3)', id LIMIT 9;
