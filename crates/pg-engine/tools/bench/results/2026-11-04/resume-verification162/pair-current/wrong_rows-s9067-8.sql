-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a32306  (rows=2)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a32306  (rows=117)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (a bool, b date);

INSERT INTO t0 (a, b) VALUES (NULL, NULL), (false, '2024-01-10'), (true, NULL), (NULL, NULL), (false, '2024-01-01'), (NULL, NULL), (true, NULL), (NULL, '2024-02-25'), (true, '2024-03-22'), (NULL, '2024-01-03'), (false, '2024-01-24'), (false, NULL);

CREATE INDEX t0_i741 ON t0 USING hash (a);

CREATE TABLE t1 (a text, b interval, c int4, d int8);

INSERT INTO t1 (a, b, c, d) VALUES ('x ', NULL, -2, 8), ('ab', '1 day', 4, NULL), ('ß', '1 mon', -2, 17);

CREATE INDEX t1_i954 ON t1 (d);

CREATE INDEX t1_i438 ON t1 (a);

CREATE INDEX t1_i177 ON t1 (d);

CREATE TABLE t2 (id int4 PRIMARY KEY, a interval, b int4, c int4[], d json);

INSERT INTO t2 (id, a, b, c, d) VALUES (7, '1 day', NULL, '{3,1,2}', '{"a": "1"}'), (28, '30 days', 5, '{2}', '{"b": [1, "a"]}'), (40, '2 hours', -1, '{3,1,2}', '{"a": {"b": 2}}'), (27, '0', 1, '{2}', '"s"'), (13, '1 mon', 8, '{5}', '{"a": "1"}'), (35, '1 mon', 6, '{5}', '[]'), (60, '2 hours', -2, '{1,1,2}', NULL), (3, '24:00:00', 9, NULL, '{"a": 1, "b": "x"}'), (11, '-1 day', 5, '{5}', '{"a": {"b": 2}}'), (20, NULL, 6, '{}', NULL), (10, '0', 3, '{2}', '{"a": {"b": 2}}'), (23, '1 day', 0, '{5}', '{"a": {"b": 2}}');

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 THEN RETURN NULL; END IF; NEW.b := coalesce(NEW.b, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t2 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER DELETE ON t2 REFERENCING OLD TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "ci3", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('Z', 0)::ct3, ARRAY[ROW('z', 1)::ct3, ROW('aa', 1)::ct3], 1), (2, NULL, ARRAY[ROW('ab', 1)::ct3, ROW('ba', NULL)::ct3], 3), (3, ROW('foo', NULL)::ct3, ARRAY[ROW('ba', 0)::ct3, ROW(NULL, 2)::ct3], 2), (4, ROW('', 1)::ct3, '{}', 3), (5, ROW('abc', 1)::ct3, NULL, 3), (6, ROW('b', -1)::ct3, ARRAY[ROW(NULL, 0)::ct3, ROW('b', 2)::ct3], 0), (7, ROW('ab', 3)::ct3, NULL, 2), (8, NULL, '{}', 0), (9, ROW(NULL, -1)::ct3, '{}', 0), (10, ROW('x ', 3)::ct3, ARRAY[ROW('10', -1)::ct3, ROW('', 0)::ct3], NULL), (11, ROW('aa', 1)::ct3, '{}', NULL), (12, ROW('a%', 0)::ct3, ARRAY[ROW('Ä', 0)::ct3, ROW(NULL, NULL)::ct3], 1);

CREATE INDEX tc3_cd ON tc3 (c DESC NULLS LAST, n);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql  STRICT AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN 3 < ALL (array_fill(p, ARRAY[3])) THEN RETURN 8;
  ELSE RETURN (('{"a": 1, "b": "x"}'::jsonb || '"s"'::jsonb)::jsonb ->> 'a')::int;
  END CASE;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN ('{}'::int4[] || '{1,1,2}'::int4[]) = array_fill(p, ARRAY[3]) THEN CASE WHEN (int4range(LEAST(12, p), GREATEST(12, p)) -|- int4range(11, p, '()')) THEN 0 ELSE (SELECT bit_or((SELECT count(*) FROM t2 t2 WHERE starts_with(md5(q), t1.a))) FROM t1 t1 WHERE '2025-02-08'::date < '2024-08-20'::date) END ELSE GREATEST(p, extract(day from '2025-09-04'::date)::int) END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id FROM t2 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id FROM t2 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT (regexp_instr('foo', 'b')) AS c0, 8 AS c1, date_trunc('month', (COALESCE('1999-09-14 13:59:00'::timestamp, '2024-11-01 05:59:00'::timestamp) + GREATEST(s2.x, s1.x) * interval '1 month')) AS c2 FROM srf0(GREATEST(2147483647, 1)) s1 LEFT JOIN srf0(s1.x) s2 USING (x) WHERE (s2.x, (jsonb_strip_nulls('{"b": [1, "a"]}'::jsonb) ->> 'b')) > ((-4), trim(trim('a,b')));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2025-03-09 01:30:00-05', 'infinity', '1999-12-31', '1 mon -30 days', '1e30', NULL, 2), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-10-26 00:30:00+00', '2025-10-26 02:30:00', '1999-12-31', 'infinity', '-1e-20', '0.125', 5), (3, 'null', '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '2024-02-29 00:00:00', '2000-03-01', '-1 mon 2 days', 'NaN', NULL, 8), (4, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '0001-01-01 00:00:00 BC', '4713-01-01 BC', 'infinity', '-1e-20', '-0', 10), (5, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2025-10-26 01:30:00+00', '2024-02-29 00:00:00', '4713-01-01 BC', '1 mon -30 days', '-2.5', NULL, -3), (6, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-10-26 00:30:00+00', '2025-12-31 23:59:59.999999', '2000-03-01', 'P1DT12H', '0.5', NULL, NULL), (7, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-03-09 03:30:00-04', '2025-03-09 02:30:00', '2024-02-29', '0', '2.5', NULL, 5), (8, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-10-26 01:30:00+00', NULL, '1999-12-31', '23:59:59.999999', '0.5', '-3', NULL);

INSERT INTO tp4 VALUES (0, '4713-01-01 BC', 'b', '-7'), (3, 'infinity', 'b', '-1e-20'), (17, '2000-03-01', 'c', 'Infinity'), (24, '2025-03-09', 'a', '0.5');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "pg_c_utf8", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 5, 'Ä', '-0.5', '178000000 years', '(1,5]', '{}', 'Fat cats! FAT rats? fat-free', NULL), (2, 13, NULL, '-1e40', '-1 mon 30 days', '[3,)', '{(,0],[10,)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(-2, NULL)::r5ct, ROW(-1, '')::r5ct, ROW(2, 'o')::r5ct]), (3, 14, 'ö', '-Infinity', '-2147483648 days', '(,)', '{(,0],[10,)}', NULL, ARRAY[ROW(0, 'E')::r5ct, ROW(4, 'c')::r5ct, ROW(2, 'h')::r5ct]), (4, 14, 'ch', 'NaN', NULL, '[1,5)', '{[1,2],[2,3)}', 'The quick brown fox jumps over the lazy dog', NULL), (5, 6, 'ss', '-99999999999999999999999999999999999.999999', '1 year -1 sec', 'empty', '{empty}', 'PostgreSQL full text search: lexemes & weights', '{}'), (6, 13, '10', '123456789012345678901234567890.123456789', '1 year -1 sec', NULL, '{[1,2],[2,3)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(0, NULL)::r5ct]), (7, 10, 'a', '-Infinity', '1 day 00:00:00.000001', '(,)', '{[1,3),[5,7)}', 'running runners ran; the runner runs', NULL), (8, 8, 'a', '-0.5', '2147483647 days', '[0,10)', '{[0,100)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(3, NULL)::r5ct, ROW(5, '-a')::r5ct]), (9, 10, NULL, '0', '-178000000 years', '[1,5)', '{[1,2],[2,3)}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (10, -2, 'İ', 'NaN', '720:00:00', '[3,)', '{[0,100)}', 'running runners ran; the runner runs', '{}');

CREATE INDEX r5a_b ON r5a (b COLLATE "unicode");

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 16 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (11, 'a', '2025-01-01'), (1, 'b', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (22, 'x0', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob1', 1, NULL, '{"seats": 1.50}'), (2, 'u2@x.org', 'dee2', 0, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', '3', 3, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', 'Éva4', 4, NULL, NULL), (5, 'u5@x.org', 'Cid5', 5, '{b,NULL}', NULL), (6, 'u6@x.org', 'Cid6', 6, '{b,NULL}', NULL), (7, 'u7@x.org', 'Cid7', 0, '{a}', '{}'), (8, 'u8@x.org', 'dee8', 0, '{a}', NULL), (9, 'u9@x.org', 'Cid9', 9, '{a}', '[1, {"a": null}]'), (10, 'u10@x.org', 'ann10', 10, '{b,NULL}', '{"seats": 1.50}'), (11, 'u11@x.org', 'bob 11', 11, '{}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (6, 0, 'paid', '2025-02-18', NULL);

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

INSERT INTO r7k VALUES (1, 1, 146, 'Note 3', 3.333, NULL, '2025-03-09 06:59:59+00'), (2, 1, 84, 'é', 0.5, NULL, NULL), (3, 6, 129, 'note 3', 1000, -0, '2025-03-09 06:59:59+00'), (4, 1, 174, 'note 3', 3.333, NULL, '2025-03-09 06:59:59+00'), (5, 1, -50, '', 0, 'NaN', '2024-02-29 12:00:00+00'), (6, 1, -22, 'note_4', 1000, -0, '2025-03-09 06:59:59+00'), (7, 1, 134, 'Note 3', 0.5, 1e300, '2025-11-02 05:30:00+00'), (8, 1, 155, '', NULL, 0.5, '2025-11-02 05:30:00+00'), (9, 1, 43, 'é', 0, -0, '2024-02-29 12:00:00+00'), (10, 1, NULL, 'NOTE 3', NULL, 1e300, NULL), (11, 9, 190, 'é', 0, 'Infinity', '2025-03-09 06:59:59+00'), (12, 1, -45, 'é', 0, 0.5, '2024-02-29 12:00:00+00'), (13, 1, 133, 'NOTE 3', NULL, -0, NULL), (14, 1, 59, 'note 1', NULL, 2.25, NULL), (15, 1, 191, 'note 3', -12.125, 1e300, '2025-03-09 06:59:59+00'), (16, 5, 188, 'n%5', 0, 'NaN', '2025-03-09 06:59:59+00'), (17, 2, 63, 'note_4', NULL, 'Infinity', '2025-03-09 07:00:00+00'), (18, 2, NULL, '', -12.125, 2.25, '2025-03-09 06:59:59+00'), (19, 7, 35, 'note_4', NULL, 1e300, NULL), (20, 1, 16, 'note 3', NULL, 'NaN', '2025-03-09 06:59:59+00'), (21, 1, -27, 'Note 3', -12.125, -0, NULL), (22, 9, -16, 'note 1', 0.5, 0.5, '2025-03-09 06:59:59+00'), (23, 1, 175, 'note 3', 1000, 0.5, '2025-11-02 05:30:00+00'), (24, 1, 99, 'é', NULL, 'NaN', '2024-02-29 12:00:00+00'), (25, 1, 146, 'note 2', NULL, 'NaN', '2024-02-29 12:00:00+00'), (26, 2, 192, 'Note 3', 0.5, 1e300, '2025-03-09 06:59:59+00'), (27, 1, 190, 'note 1', NULL, 1e300, NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3);

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

INSERT INTO r7t VALUES (1, 'ä', 'ä'), (2, '9', '9'), (3, '9', '9'), (4, 'z', 'z'), (5, '', ''), (6, 'a', 'a'), (7, 'é', 'é'), (8, 'ß', 'ß'), (9, '10', '10'), (10, 'ä b', 'ä b'), (11, 'ß', 'ß'), (12, 'E', 'E');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, -2, -2.25, 'B', '2025-01-01'::timestamp + interval '33 hours'), (2, 1, 12, NULL, 'z', '2025-01-01'::timestamp + interval '102 hours'), (3, 4, 7, 1.50, 'ab', '2025-01-01'::timestamp + interval '54 hours'), (4, 3, -2, 1.50, 'z', '2025-01-01'::timestamp + interval '26 hours'), (5, 3, 10, NULL, '', '2025-01-01'::timestamp + interval '18 hours'), (6, 3, 1, 1.50, 'ab', '2025-01-01'::timestamp + interval '96 hours'), (7, 2, 1, NULL, 'a', '2025-01-01'::timestamp + interval '36 hours'), (8, 2, 18, 1.50, 'b', '2025-01-01'::timestamp + interval '18 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 6), (1, 4, 9), (2, 5, 5), (2, 5, 7), (4, 1, 7), (4, 4, 6), (5, 3, 3), (5, 2, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{[3,5), [7, 9)}', '(2024-12-31,2025-01-05]', '(,0]'), (2, '(-3,2]', '{(,-3)}', NULL, NULL), (3, '(8,13]', '{(,8)}', NULL, NULL), (4, '[6,10)', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (5, NULL, NULL, '(2024-12-31,2025-01-05]', '[0,0]'), (6, 'empty', '{}', 'empty', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'a rat ran'), (2, 'a rat ran'), (3, 'foxes and dogs'), (4, 'running rats'), (5, ''), (6, 'foxes and dogs'), (7, 'über café naïve'), (8, 'über café naïve'), (9, 'dog-eat-dog world'), (10, 'the quick brown fox');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (4, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (5, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3424) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2684) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3850) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a32306 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a32306 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 352, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 352) i;

UPDATE r9a32306 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a32306 (g, s, arr);

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a32306 WHERE arr @> '{2}'$$);
