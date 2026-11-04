-- class: wrong_rows
-- ordered: false
-- note: 10 vs 10 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Sort
-- engine:         Sort Key: n.id
-- engine:         ->  Nested Loop
-- engine:               ->  Bitmap Heap Scan on r9s s
-- engine:                     Recheck Cond: (g = 8)
-- engine:                     ->  Bitmap Index Scan on r9s_gv
-- engine:                           Index Cond: (g = 8)
-- engine:               ->  Index Scan using r9n_pkey on r9n n
-- engine:                     Index Cond: (id = s.id)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Merge Join
-- reference:         Merge Cond: (n.id = s.id)
-- reference:         ->  Index Scan using r9n_pkey on r9n n
-- reference:         ->  Sort
-- reference:               Sort Key: s.id
-- reference:               ->  Bitmap Heap Scan on r9s s
-- reference:                     Recheck Cond: (g = 8)
-- reference:                     ->  Bitmap Index Scan on r9s_gv
-- reference:                           Index Cond: (g = 8)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b numeric(10,2), g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

CREATE INDEX t0_i74 ON t0 (b);

CREATE INDEX t0_i137 ON t0 (a, id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; NEW.a := coalesce(NEW.a, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (a d3int, b text, c d3int, d json, e bool);

CREATE TABLE t1_c (extra int) INHERITS (t1);

INSERT INTO t1 (a, b, c, d, e) VALUES (8, 'abc', 6, NULL, false), (NULL, 'x ', NULL, '"s"', false), (NULL, NULL, 9, 'null', NULL), (NULL, NULL, NULL, '{"a": {"b": 2}}', true), (NULL, NULL, 5, '1', NULL), (NULL, '', NULL, NULL, NULL), (NULL, '10', NULL, NULL, NULL), (6, 'z', NULL, '{"a": 3}', true), (NULL, NULL, NULL, '{"a": {"b": 2}}', NULL), (0, 'ß', NULL, '1', false), (8, '', NULL, '1', false), (1, 'aa', 7, NULL, true), (9, '', NULL, '[]', true), (NULL, NULL, 5, '{"a": 1, "b": "x"}', false), (NULL, 'a,b', -1, NULL, false), (0, 'a_b', 0, 'null', NULL), (NULL, 'aa', -2, '[1, 2, 3]', NULL), (NULL, NULL, NULL, NULL, false), (NULL, NULL, 6, NULL, NULL), (2, 'z', -1, NULL, NULL), (NULL, 'a_b', NULL, NULL, NULL), (NULL, NULL, 2, '1', false), (NULL, NULL, 6, '{"a": {"b": 2}}', NULL), (5, NULL, NULL, NULL, true), (NULL, NULL, 9, '{"a": "1"}', false), (6, 'a', NULL, NULL, true), (6, 'B', NULL, NULL, true), (1, 'abc', 5, NULL, NULL), (4, NULL, 4, '{"b": [1, "a"]}', NULL), (NULL, 'ab', 2, '{"a": "1"}', true);

CREATE TABLE t2 (id int4 PRIMARY KEY, a bool, b int8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t2 (id, a, b) VALUES (28, false, NULL), (33, false, 2), (1, false, 16), (8, false, 4), (20, false, 13), (23, true, 15), (12, true, 2), (19, false, 5), (39, false, 16), (21, true, 17), (22, NULL, -2), (13, NULL, 9);

CREATE INDEX t2_i848 ON t2 USING hash (g);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN isfinite(NULLIF('1999-04-25'::date, '2025-02-12'::date)) THEN p ELSE p END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE STRICT AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN abs('NaN'::numeric) < round(1::numeric, 3) THEN RETURN LEAST(('2000-01-23'::date - '1999-08-14'::date), (SELECT ascii(q) FROM t0 t1 WHERE COALESCE(true, false) ORDER BY 1 LIMIT 1));
  ELSE RETURN p;
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT a, b FROM t1 WHERE a <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT a, b FROM t1 WHERE a IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS SELECT -7.125::numeric AS c0, s1.c0 AS c1, '1 day'::interval AS c2 FROM (SELECT (count(*))::int AS c0, ((1::int8 * 5) + ((-3)::int8 + 1::int8)) AS c1, min('2024-04-26 15:00:00-05'::timestamptz) AS c2 FROM t1 t2 CROSS JOIN t2 t3 GROUP BY t3.a) s1 WHERE '[1, 2, 3]'::jsonb = '[]'::jsonb;

CREATE VIEW v1 AS (SELECT (t1.id % 4) AS c0, quote_literal('Ab') AS c1, t1.a AS c2 FROM t2 t1 WHERE t1.a) EXCEPT ALL (SELECT DISTINCT (((t3.g << 1) / sign(t3.id)::int) % 2) AS c0, 'aa' AS c1, (CASE WHEN NULL::bool THEN '1'::jsonb ELSE '{"a": 1, "b": "x"}'::jsonb END || (jsonb_build_array('[1, 2, 3]'::jsonb, t2.b) -> 1)) ? 'b' AS c2 FROM (t0 t2 LEFT JOIN t0 t3 ON t2.id = t3.id) WHERE ((8, array_to_string('{a,b}'::text[], ',', '?')) <> (extract(dow from '2000-05-23'::date)::int, split_part('', ',', 1))) IS UNKNOWN);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '2024-02-29 00:00:00', '1999-12-31', '1 year 2 mons', '99999999999999999999', '-1.25', 3), (2, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '2025-10-26 02:30:00', '1999-12-31', '23:59:59.999999', '123.45', 'Infinity', 3), (3, '{}', '{}', '-infinity', '2025-11-02 01:30:00', '2025-01-01', '1 day', 'Infinity', NULL, 1), (4, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', NULL, '1999-12-31', '1 mon -30 days', 'NaN', 'Infinity', NULL), (5, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '1970-01-01 00:00:00.5', '1999-12-31', '-1 mon 2 days', '2.5', '0.125', NULL), (6, '{}', '{}', '2025-04-06 02:30:00+11', '0001-01-01 00:00:00 BC', '2025-03-09', '2 weeks 3 microseconds', '1.23456789012345678901234567890', 'NaN', 12), (7, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', NULL, '0001-01-01 00:00:00 BC', '2024-02-29', '0', NULL, NULL, 10), (8, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-04-06 02:30:00+11', '2025-03-09 02:30:00', NULL, '36 hours', '0.5', '0.125', 11), (9, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', '2025-10-26 02:30:00', '2000-03-01', NULL, '2.5', '-3', 0), (10, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '-infinity', '0001-01-01 00:00:00 BC', NULL, '1 year 2 mons', '2.5', 'Infinity', 3), (11, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-10-26 00:30:00+00', '2000-01-01 00:00:00', '4713-01-01 BC', '0', NULL, '-0', -1), (12, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', NULL, '1970-01-01 00:00:00.5', '2000-03-01', '1 day', '-7', '2', 9), (13, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-04-06 02:30:00+11', '2024-02-29 00:00:00', '2025-01-01', '1 mon -30 days', 'NaN', '2', -1);

INSERT INTO tp4 VALUES (-5, '2024-02-29', 'c', '-1e-20'), (14, '-infinity', 'ab', NULL), (14, '2000-03-01', 'b', NULL), (13, NULL, 'b', '0.5'), (-2, 'infinity', 'd', '0.5'), (17, '2025-01-01', 'a', NULL), (0, '-infinity', 'ab', '-1e-20'), (0, '2025-01-01', 'd', '99999999999999999999'), (17, '2000-03-01', 'b', NULL), (5, '2025-03-09', 'ab', '0'), (4, '1999-12-31', 'c', '1e30'), (5, 'infinity', 'c', '2.5'), (8, '-infinity', 'a', '99999999999999999999'), (21, '4713-01-01 BC', 'c', '-0.0'), (20, '2000-03-01', 'a', '-7'), (1, '2025-03-09', NULL, '2.5');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 2, 'z', '7', NULL, 'empty', '{empty}', NULL, NULL), (2, 13, 'AB', '7', '178000000 years', NULL, '{[1,2],[2,3)}', '', ARRAY[ROW(2, '9')::r5ct, ROW(1, '10')::r5ct, ROW(3, ' a')::r5ct]), (3, 1, 'e', '-0.5', '178000000 years', '[-3,0]', '{}', 'running runners ran; the runner runs', '{}'), (4, 17, '', NULL, '1 day 00:00:00.000001', 'empty', '{[1,3),[5,7)}', 'The quick brown fox jumps over the lazy dog', NULL), (5, 17, 'A', '7', '720:00:00', '(,)', '{empty}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', '{}');

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 3 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 15 WITH LOCAL CHECK OPTION;

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

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (15, 'a', 'infinity'), (-3, 'b', '2024-02-29'), (6, 'c', '2024-02-29'), (-2, 'd', NULL), (2, 'a', '2024-02-29'), (17, 'b', '2024-02-29'), (2, 'c', '2025-01-01'), (1, 'd', '2025-01-01'), (2, 'a', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL), (31, 'x1', NULL), (22, 'x2', NULL), (32, 'x3', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob1', 1, '{}', NULL), (2, 'u2@x.org', 'zoë2', 2, '{a}', '[1, {"a": null}]'), (3, 'u3@x.org', 'ann3', 3, '{a}', NULL), (4, 'u4@x.org', 'bob4', 4, '{b,NULL}', NULL), (5, 'u5@x.org', 'ann5', 5, '{}', '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', 'Cid6', 0, '{a,b}', '[1, {"a": null}]'), (7, 'u7@x.org', 'zoë7', 0, '{b,NULL}', '[1, {"a": null}]'), (8, 'u8@x.org', 'ann8', 8, '{a}', '{}'), (9, 'u9@x.org', '9', 0, NULL, NULL), (10, 'u10@x.org', 'dee10', 10, NULL, '{"plan": "free", "tags": [1, 2]}'), (11, 'u11@x.org', 'bob 11', 11, '{}', '{"plan": "pro", "seats": 3}'), (12, 'u12@x.org', 'bob 12', 0, '{}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (7, 100, 'new', '2025-02-25', NULL), (10, 100, 'void', '2025-02-03', '{"items": []}'), (3, 100, 'void', '2025-02-13', NULL), (5, -1.50, 'new', '2025-02-21', '{"items": [{"sku": "a", "qty": 2}]}'), (10, 100, 'void', '2025-02-18', NULL), (2, 12345678.90, 'void', '2025-02-01', NULL), (3, 12345678.90, 'paid', '2025-02-08', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 100, 'new', '2025-02-15', '{"items": []}'), (12, -1.50, 'void', '2025-02-02', NULL), (7, 12345678.90, 'void', '2025-02-02', NULL), (12, -1.50, 'new', '2025-02-27', '{"gift": true}'), (12, 9.99, 'new', '2025-02-24', '{"items": []}'), (8, NULL, 'void', '2025-02-04', '{"items": []}'), (6, 100, 'void', '2025-02-13', '{"gift": true}'), (5, 0, 'void', '2025-02-06', NULL), (4, NULL, 'void', '2025-02-24', '{"items": [{"sku": "a", "qty": 2}]}'), (11, 0, 'paid', '2025-02-07', NULL), (6, -1.50, 'void', '2025-02-18', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, 110, 'note 2', -12.125, 1e300, '2025-03-09 07:00:00+00'), (2, 1, 17, 'note_4', 1000, 0.5, '2025-03-09 07:00:00+00'), (3, 7, 32, 'n%5', NULL, NULL, '2024-02-29 12:00:00+00'), (4, 4, 197, 'NOTE 3', -12.125, 1e300, '2025-03-09 06:59:59+00'), (5, 8, -13, 'note 2', 0, 0.5, '2024-02-29 12:00:00+00'), (6, 1, NULL, 'é', NULL, 2.25, '2025-03-09 06:59:59+00'), (7, 1, 38, 'NOTE 3', 3.333, 2.25, '2024-02-29 12:00:00+00'), (8, 5, 181, 'n%5', 1000, NULL, NULL), (9, 1, 25, '', -12.125, 0.5, '2025-11-02 05:30:00+00'), (10, 1, 178, 'Note 3', 0.5, 2.25, '2024-02-29 12:00:00+00'), (11, 1, NULL, 'note 1', NULL, -0, '2024-02-29 12:00:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10), (9, 'us', 17);

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

INSERT INTO r7t VALUES (1, 'z', 'z'), (2, 'ae', 'ae'), (3, 'ä', 'ä'), (4, 'A', 'A'), (5, 'ae', 'ae'), (6, '10', '10'), (7, 'ß', 'ß'), (8, '10', '10'), (9, 'A', 'A'), (10, '10', '10'), (11, 'A', 'A'), (12, 'ss', 'ss'), (13, 'ß', 'ß'), (14, 'ae', 'ae');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, NULL, 10, 'ab', '2025-01-01'::timestamp + interval '126 hours'), (2, 3, 19, 1.50, 'B', '2025-01-01'::timestamp + interval '159 hours'), (3, 1, NULL, 1.50, 'ab', '2025-01-01'::timestamp + interval '0 hours'), (4, 4, 8, 1.50, '', '2025-01-01'::timestamp + interval '102 hours'), (5, 4, 14, 10, '', '2025-01-01'::timestamp + interval '39 hours'), (6, 1, 8, 1.50, 'ab', '2025-01-01'::timestamp + interval '96 hours'), (7, 4, -4, 3.75, NULL, '2025-01-01'::timestamp + interval '24 hours'), (8, 1, 8, 1.50, 'b', '2025-01-01'::timestamp + interval '69 hours'), (9, 2, 19, 3.75, 'z', '2025-01-01'::timestamp + interval '21 hours'), (10, 2, 3, 1.50, 'b', '2025-01-01'::timestamp + interval '9 hours'), (11, 4, 16, 1.50, 'ab', '2025-01-01'::timestamp + interval '87 hours'), (12, 2, 20, 1.50, 'B', '2025-01-01'::timestamp + interval '22 hours'), (13, 1, 4, NULL, 'ab', '2025-01-01'::timestamp + interval '150 hours'), (14, 1, 13, -2.25, '', '2025-01-01'::timestamp + interval '2 hours'), (15, 3, 15, 3.75, 'b', '2025-01-01'::timestamp + interval '144 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 5, 8), (2, 5, 7), (3, 4, 8), (3, 5, 5), (5, 3, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[0,6)', NULL, '(2024-12-31,2025-01-05]', '(,0]'), (2, '[-2,-1)', '{}', 'empty', NULL), (3, '[-3,0)', '{}', '[2025-01-01,2025-02-01)', NULL), (4, '[-1,)', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (5, NULL, '{[7,9), [11, 13)}', '[2025-03-01,infinity)', '(,0]'), (6, 'empty', '{(,-2)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (7, 'empty', '{}', 'empty', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, 'brown''s fox''s den'), (3, 'über café naïve'), (4, 'a rat ran'), (5, 'über café naïve'), (6, '');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (6, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3063) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2059) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2991) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = force_generic_plan; FOR i IN 0..10 LOOP s := s + coalesce((SELECT coalesce(max(v), 0) FROM r9s WHERE g = 99 AND v > 100 / (i - 7)), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

SET work_mem = '64kB';

EXPLAIN (COSTS OFF) SELECT id, s FROM r8big ORDER BY s, id LIMIT 7 OFFSET 220;

RESET work_mem;

RESET enable_hashagg;

RESET enable_sort;

RESET enable_mergejoin;

RESET hash_mem_multiplier;

RESET enable_hashjoin;

SELECT lo_from_bytea(948524, '\x68656c6c6f20776f726c64')::int8 - 948524;

SELECT lo_get(948524), lo_get(948524, 1, 1);

BEGIN;

SELECT lo_open(948524, x'40000'::int);

SELECT loread(0, 3);

SELECT lo_lseek(0, -2, 1);

SELECT lowrite(0, '\x5a5a');

SELECT lo_tell(0), lo_lseek64(0, 0, 2);

SELECT loread(0, 100);

SELECT lo_close(0);

COMMIT;

SELECT lo_get(948524);

SELECT oid::int8 - 948524, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = 948524;

SELECT lo_create(948524);

SELECT lo_unlink(948524);

SELECT lo_get(948524);

CREATE TABLE r9c63780 (id int PRIMARY KEY, a int, b text) WITH (autovacuum_enabled = off);

CREATE TABLE r9c63780log (n serial, op text, id int, b text);

CREATE FUNCTION r9c63780fb() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN IF NEW.a < -3 THEN RETURN NULL; END IF;  NEW.b := upper(NEW.b); RETURN NEW; END $f$;

CREATE FUNCTION r9c63780fa() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c63780log (op, id, b) VALUES (TG_WHEN || ' ' || TG_LEVEL, NEW.id, NEW.b); RETURN NULL; END $f$;

CREATE FUNCTION r9c63780fs() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c63780log (op, id, b) SELECT 'stmt', count(*), max(b) FROM nt; RETURN NULL; END $f$;

CREATE TRIGGER r9c63780b BEFORE INSERT ON r9c63780 FOR EACH ROW EXECUTE FUNCTION r9c63780fb();

CREATE TRIGGER r9c63780a AFTER INSERT ON r9c63780 FOR EACH ROW EXECUTE FUNCTION r9c63780fa();

INSERT INTO r9c63780 (id, a, b) VALUES (7, 1, 'pre'), (1200, 2, 'pre2');

COPY r9c63780 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
7	44	b7
8	1	b8
9	8	b9
10	15	b10
11	22	b11
12	29	b12
13	36	b13
14	43	b14
15	0	b15
16	7	b16
17	14	\N
18	21	b18
19	28	b19
20	35	b20
21	42	b21
22	-1	b22
23	6	b23
24	13	b24
25	20	b25
26	27	b26
27	34	b27
28	41	b28
29	-2	b29
30	5	b30
31	12	b31
32	19	b32
33	26	b33
34	33	\N
35	40	b35
36	-3	b36
37	4	b37
38	11	b38
39	18	b39
40	25	b40
41	32	b41
42	39	b42
43	-4	b43
44	3	b44
45	10	b45
46	17	b46
47	24	b47
48	31	b48
49	38	b49
50	-5	b50
51	2	\N
52	9	b52
53	16	b53
54	23	b54
55	30	b55
56	37	b56
5000	44	b57
58	1	b58
59	8	b59
60	15	b60
61	22	b61
62	29	b62
63	36	b63
64	43	b64
65	0	b65
66	7	b66
67	14	b67
68	21	\N
69	28	b69
70	35	b70
71	42	b71
72	-1	b72
73	6	b73
74	13	b74
75	20	b75
76	27	b76
77	34	b77
78	41	b78
79	-2	b79
80	5	b80
81	12	b81
82	19	b82
83	26	b83
84	33	b84
85	40	\N
86	-3	b86
87	4	b87
88	11	b88
89	18	b89
90	25	b90
91	32	b91
92	39	b92
93	-4	b93
94	3	b94
95	10	b95
96	17	b96
97	24	b97
98	31	b98
99	38	b99
100	-5	b100
101	2	b101
102	9	\N
103	16	b103
104	23	b104
105	30	b105
106	37	b106
107	44	b107
108	1	b108
109	8	b109
110	15	b110
111	22	b111
112	29	b112
113	36	b113
114	43	b114
115	0	b115
116	7	b116
117	14	b117
118	21	b118
119	28	\N
120	35	b120
121	42	b121
122	-1	b122
123	6	b123
124	13	b124
125	20	b125
126	27	b126
127	34	b127
128	41	b128
129	-2	b129
130	5	b130
131	12	b131
132	19	b132
133	26	b133
134	33	b134
135	40	b135
136	-3	\N
137	4	b137
138	11	b138
139	18	b139
140	25	b140
141	32	b141
142	39	b142
143	-4	b143
144	3	b144
145	10	b145
146	17	b146
147	24	b147
148	31	b148
149	38	b149
150	-5	b150
151	2	b151
152	9	b152
153	16	\N
154	23	b154
155	30	b155
156	37	b156
157	44	b157
158	1	b158
159	8	b159
160	15	b160
161	22	b161
162	29	b162
163	36	b163
164	43	b164
165	0	b165
166	7	b166
167	14	b167
168	21	b168
169	28	b169
170	35	\N
171	42	b171
172	-1	b172
173	6	b173
174	13	b174
175	20	b175
176	27	b176
177	34	b177
178	41	b178
179	-2	b179
180	5	b180
181	12	b181
182	19	b182
183	26	b183
184	33	b184
185	40	b185
186	-3	b186
187	4	\N
188	11	b188
189	18	b189
190	25	b190
191	32	b191
192	39	b192
193	-4	b193
194	3	b194
195	10	b195
196	17	b196
197	24	b197
198	31	b198
199	38	b199
200	-5	b200
201	2	b201
202	9	b202
203	16	b203
204	23	\N
205	30	b205
206	37	b206
207	44	b207
208	1	b208
209	8	b209
210	15	b210
211	22	b211
212	29	b212
213	36	b213
214	43	b214
215	0	b215
216	7	b216
217	14	b217
218	21	b218
219	28	b219
220	35	b220
221	42	\N
222	-1	b222
223	6	b223
224	13	b224
225	20	b225
226	27	b226
227	34	b227
228	41	b228
229	-2	b229
230	5	b230
231	12	b231
232	19	b232
233	26	b233
234	33	b234
235	40	b235
236	-3	b236
237	4	b237
238	11	\N
239	18	b239
240	25	b240
241	32	b241
242	39	b242
243	-4	b243
244	3	b244
245	10	b245
246	17	b246
247	24	b247
248	31	b248
249	38	b249
250	-5	b250
251	2	b251
252	9	b252
253	16	b253
254	23	b254
255	30	\N
256	37	b256
257	44	b257
258	1	b258
259	8	b259
260	15	b260
261	22	b261
262	29	b262
263	36	b263
264	43	b264
265	0	b265
266	7	b266
267	14	b267
268	21	b268
269	28	b269
270	35	b270
271	42	b271
272	-1	\N
273	6	b273
274	13	b274
275	20	b275
276	27	b276
277	34	b277
278	41	b278
279	-2	b279
280	5	b280
281	12	b281
282	19	b282
283	26	b283
284	33	b284
285	40	b285
286	-3	b286
287	4	b287
288	11	b288
289	18	\N
290	25	b290
291	32	b291
292	39	b292
293	-4	b293
294	3	b294
295	10	b295
296	17	b296
297	24	b297
298	31	b298
299	38	b299
300	-5	b300
301	2	b301
302	9	b302
303	16	b303
304	23	b304
305	30	b305
306	37	\N
307	44	b307
308	1	b308
309	8	b309
310	15	b310
311	22	b311
312	29	b312
313	36	b313
314	43	b314
315	0	b315
316	7	b316
317	14	b317
318	21	b318
319	28	b319
320	35	b320
321	42	b321
322	-1	b322
323	6	\N
324	13	b324
325	20	b325
326	27	b326
327	34	b327
328	41	b328
329	-2	b329
330	5	b330
331	12	b331
332	19	b332
333	26	b333
334	33	b334
335	40	b335
336	-3	b336
337	4	b337
338	11	b338
339	18	b339
340	25	\N
341	32	b341
342	39	b342
343	-4	b343
344	3	b344
345	10	b345
346	17	b346
347	24	b347
348	31	b348
349	38	b349
350	-5	b350
351	2	b351
352	9	b352
353	16	b353
354	23	b354
355	30	b355
356	37	b356
357	44	\N
358	1	b358
359	8	b359
360	15	b360
361	22	b361
362	29	b362
363	36	b363
364	43	b364
365	0	b365
366	7	b366
367	14	b367
368	21	b368
369	28	b369
370	35	b370
371	42	b371
372	-1	b372
373	6	b373
374	13	\N
375	20	b375
376	27	b376
377	34	b377
378	41	b378
379	-2	b379
380	5	b380
381	12	b381
382	19	b382
383	26	b383
384	33	b384
385	40	b385
386	-3	b386
387	4	b387
388	11	b388
389	18	b389
390	25	b390
391	32	\N
392	39	b392
393	-4	b393
394	3	b394
395	10	b395
396	17	b396
397	24	b397
398	31	b398
399	38	b399
400	-5	b400
401	2	b401
402	9	b402
403	16	b403
404	23	b404
405	30	b405
406	37	b406
407	44	b407
408	1	\N
409	8	b409
410	15	b410
411	22	b411
412	29	b412
413	36	b413
414	43	b414
415	0	b415
416	7	b416
417	14	b417
418	21	b418
419	28	b419
420	35	b420
421	42	b421
422	-1	b422
423	6	b423
424	13	b424
425	20	\N
426	27	b426
427	34	b427
428	41	b428
429	-2	b429
430	5	b430
431	12	b431
432	19	b432
433	26	b433
434	33	b434
435	40	b435
436	-3	b436
437	4	b437
438	11	b438
439	18	b439
440	25	b440
441	32	b441
442	39	\N
443	-4	b443
444	3	b444
445	10	b445
446	17	b446
447	24	b447
448	31	b448
449	38	b449
450	-5	b450
451	2	b451
452	9	b452
453	16	b453
454	23	b454
455	30	b455
456	37	b456
457	44	b457
458	1	b458
459	8	\N
460	15	b460
461	22	b461
462	29	b462
463	36	b463
464	43	b464
465	0	b465
466	7	b466
467	14	b467
468	21	b468
469	28	b469
470	35	b470
471	42	b471
472	-1	b472
473	6	b473
474	13	b474
475	20	b475
476	27	\N
477	34	b477
478	41	b478
479	-2	b479
480	5	b480
481	12	b481
482	19	b482
483	26	b483
484	33	b484
485	40	b485
486	-3	b486
487	4	b487
488	11	b488
489	18	b489
490	25	b490
491	32	b491
492	39	b492
493	-4	\N
494	3	b494
495	10	b495
496	17	b496
497	24	b497
498	31	b498
499	38	b499
500	-5	b500
501	2	b501
502	9	b502
503	16	b503
504	23	b504
505	30	b505
506	37	b506
507	44	b507
508	1	b508
509	8	b509
510	15	\N
511	22	b511
512	29	b512
513	36	b513
514	43	b514
515	0	b515
516	7	b516
517	14	b517
518	21	b518
519	28	b519
520	35	b520
521	42	b521
522	-1	b522
523	6	b523
524	13	b524
525	20	b525
526	27	b526
527	34	\N
528	41	b528
529	-2	b529
530	5	b530
531	12	b531
532	19	b532
533	26	b533
534	33	b534
535	40	b535
536	-3	b536
537	4	b537
538	11	b538
539	18	b539
540	25	b540
541	32	b541
542	39	b542
543	-4	b543
544	3	\N
545	10	b545
546	17	b546
547	24	b547
548	31	b548
549	38	b549
550	-5	b550
551	2	b551
552	9	b552
553	16	b553
554	23	b554
555	30	b555
556	37	b556
557	44	b557
558	1	b558
559	8	b559
560	15	b560
561	22	\N
562	29	b562
563	36	b563
564	43	b564
565	0	b565
566	7	b566
567	14	b567
568	21	b568
569	28	b569
570	35	b570
571	42	b571
572	-1	b572
573	6	b573
574	13	b574
575	20	b575
576	27	b576
577	34	b577
578	41	\N
579	-2	b579
580	5	b580
581	12	b581
582	19	b582
583	26	b583
584	33	b584
585	40	b585
586	-3	b586
587	4	b587
588	11	b588
589	18	b589
590	25	b590
591	32	b591
592	39	b592
593	-4	b593
594	3	b594
595	10	\N
596	17	b596
597	24	b597
598	31	b598
599	38	b599
600	-5	b600
601	2	b601
602	9	b602
603	16	b603
604	23	b604
605	30	b605
606	37	b606
607	44	b607
608	1	b608
609	8	b609
610	15	b610
611	22	b611
612	29	\N
613	36	b613
614	43	b614
615	0	b615
616	7	b616
617	14	b617
618	21	b618
619	28	b619
620	35	b620
621	42	b621
622	-1	b622
623	6	b623
624	13	b624
625	20	b625
626	27	b626
627	34	b627
628	41	b628
629	-2	\N
630	5	b630
631	12	b631
632	19	b632
633	26	b633
634	33	b634
635	40	b635
636	-3	b636
637	4	b637
638	11	b638
639	18	b639
640	25	b640
641	32	b641
642	39	b642
643	-4	b643
644	3	b644
645	10	b645
646	17	\N
647	24	b647
648	31	b648
649	38	b649
650	-5	b650
651	2	b651
652	9	b652
653	16	b653
654	23	b654
655	30	b655
656	37	b656
657	44	b657
658	1	b658
659	8	b659
660	15	b660
661	22	b661
662	29	b662
663	36	\N
664	43	b664
665	0	b665
666	7	b666
667	14	b667
668	21	b668
669	28	b669
670	35	b670
671	42	b671
672	-1	b672
673	6	b673
674	13	b674
675	20	b675
676	27	b676
677	34	b677
678	41	b678
679	-2	b679
680	5	\N
681	12	b681
682	19	b682
683	26	b683
684	33	b684
685	40	b685
686	-3	b686
687	4	b687
688	11	b688
689	18	b689
690	25	b690
691	32	b691
692	39	b692
693	-4	b693
694	3	b694
695	10	b695
696	17	b696
697	24	\N
698	31	b698
699	38	b699
700	-5	b700
701	2	b701
702	9	b702
703	16	b703
704	23	b704
705	30	b705
706	37	b706
707	44	b707
708	1	b708
709	8	b709
710	15	b710
711	22	b711
712	29	b712
713	36	b713
714	43	\N
715	0	b715
716	7	b716
717	14	b717
718	21	b718
719	28	b719
720	35	b720
721	42	b721
722	-1	b722
723	6	b723
724	13	b724
725	20	b725
726	27	b726
727	34	b727
728	41	b728
729	-2	b729
730	5	b730
731	12	\N
732	19	b732
733	26	b733
734	33	b734
735	40	b735
736	-3	b736
737	4	b737
738	11	b738
739	18	b739
740	25	b740
741	32	b741
742	39	b742
743	-4	b743
744	3	b744
745	10	b745
746	17	b746
747	24	b747
748	31	\N
749	38	b749
750	-5	b750
751	2	b751
752	9	b752
753	16	b753
754	23	b754
755	30	b755
756	37	b756
757	44	b757
758	1	b758
759	8	b759
760	15	b760
761	22	b761
762	29	b762
763	36	b763
764	43	b764
765	0	\N
766	7	b766
767	14	b767
768	21	b768
769	28	b769
770	35	b770
771	42	b771
772	-1	b772
773	6	b773
774	13	b774
775	20	b775
776	27	b776
777	34	b777
778	41	b778
779	-2	b779
780	5	b780
781	12	b781
782	19	\N
783	26	b783
784	33	b784
785	40	b785
786	-3	b786
787	4	b787
788	11	b788
789	18	b789
790	25	b790
791	32	b791
792	39	b792
793	-4	b793
794	3	b794
795	10	b795
796	17	b796
797	24	b797
798	31	b798
799	38	\N
800	-5	b800
801	2	b801
802	9	b802
803	16	b803
804	23	b804
805	30	b805
806	37	b806
807	44	b807
808	1	b808
809	8	b809
810	15	b810
811	22	b811
812	29	b812
813	36	b813
814	43	b814
815	0	b815
816	7	\N
817	14	b817
818	21	b818
819	28	b819
820	35	b820
821	42	b821
822	-1	b822
823	6	b823
824	13	b824
825	20	b825
826	27	b826
827	34	b827
828	41	b828
829	-2	b829
830	5	b830
831	12	b831
832	19	b832
833	26	\N
834	33	b834
835	40	b835
836	-3	b836
837	4	b837
838	11	b838
839	18	b839
840	25	b840
841	32	b841
842	39	b842
843	-4	b843
844	3	b844
845	10	b845
846	17	b846
847	24	b847
848	31	b848
849	38	b849
850	-5	\N
851	2	b851
852	9	b852
853	16	b853
854	23	b854
855	30	b855
856	37	b856
857	44	b857
858	1	b858
859	8	b859
860	15	b860
861	22	b861
862	29	b862
863	36	b863
864	43	b864
865	0	b865
866	7	b866
867	14	\N
868	21	b868
869	28	b869
870	35	b870
871	42	b871
872	-1	b872
873	6	b873
874	13	b874
875	20	b875
876	27	b876
877	34	b877
878	41	b878
879	-2	b879
880	5	b880
881	12	b881
882	19	b882
883	26	b883
884	33	\N
885	40	b885
886	-3	b886
887	4	b887
888	11	b888
889	18	b889
890	25	b890
891	32	b891
892	39	b892
893	-4	b893
894	3	b894
895	10	b895
896	17	b896
897	24	b897
898	31	b898
899	38	b899
900	-5	b900
901	2	\N
902	9	b902
903	16	b903
904	23	b904
905	30	b905
906	37	b906
907	44	b907
908	1	b908
909	8	b909
910	15	b910
911	22	b911
912	29	b912
913	36	b913
914	43	b914
915	0	b915
916	7	b916
917	14	b917
918	21	\N
919	28	b919
920	35	b920
921	42	b921
922	-1	b922
923	6	b923
924	13	b924
925	20	b925
926	27	b926
927	34	b927
928	41	b928
929	-2	b929
930	5	b930
\.

DROP TABLE r9c63780log;

DROP TABLE r9c63780;

DROP FUNCTION r9c63780fb(), r9c63780fa(), r9c63780fs();

CREATE TABLE r9c4248 (id int, a int, b text) PARTITION BY RANGE (id);

CREATE TABLE r9c4248_1 PARTITION OF r9c4248 FOR VALUES FROM (MINVALUE) TO (500) WITH (autovacuum_enabled = off);

CREATE TABLE r9c4248_2 PARTITION OF r9c4248 FOR VALUES FROM (500) TO (1500) WITH (autovacuum_enabled = off);

CREATE TABLE r9c4248_3 PARTITION OF r9c4248 DEFAULT WITH (autovacuum_enabled = off);

BEGIN;

COPY r9c4248 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
7	44	b7
8	1	b8
9	8	b9
10	15	b10
11	22	b11
12	29	b12
13	36	b13
14	43	b14
15	0	b15
16	7	b16
17	14	\N
18	21	b18
19	28	b19
20	35	b20
21	42	b21
22	-1	b22
23	6	b23
24	13	b24
25	20	b25
26	27	b26
27	34	b27
28	41	b28
29	-2	b29
30	5	b30
31	12	b31
32	19	b32
33	26	b33
34	33	\N
35	40	b35
36	-3	b36
37	4	b37
38	11	b38
39	18	b39
40	25	b40
41	32	b41
42	39	b42
43	-4	b43
44	3	b44
45	10	b45
46	17	b46
47	24	b47
48	31	b48
49	38	b49
50	-5	b50
51	2	\N
52	9	b52
53	16	b53
54	23	b54
55	30	b55
56	37	b56
57	44	b57
58	1	b58
59	8	b59
60	15	b60
61	22	b61
62	29	b62
63	36	b63
64	43	b64
65	0	b65
66	7	b66
67	14	b67
68	21	\N
69	28	b69
70	35	b70
71	42	b71
72	-1	b72
73	6	b73
74	13	b74
75	20	b75
76	27	b76
77	34	b77
78	41	b78
79	-2	b79
80	5	b80
81	12	b81
82	19	b82
83	26	b83
84	33	b84
85	40	\N
86	-3	b86
87	4	b87
88	11	b88
89	18	b89
90	25	b90
91	32	b91
92	39	b92
93	-4	b93
94	3	b94
95	10	b95
96	17	b96
97	24	b97
98	31	b98
99	38	b99
100	-5	b100
101	2	b101
102	9	\N
103	16	b103
104	23	b104
105	30	b105
106	37	b106
107	44	b107
108	1	b108
109	8	b109
110	15	b110
111	22	b111
112	29	b112
113	36	b113
114	43	b114
115	0	b115
116	7	b116
117	14	b117
118	21	b118
119	28	\N
120	35	b120
121	42	b121
122	-1	b122
123	6	b123
124	13	b124
125	20	b125
126	27	b126
127	34	b127
128	41	b128
129	-2	b129
130	5	b130
131	12	b131
132	19	b132
133	26	b133
134	33	b134
135	40	b135
136	-3	\N
137	4	b137
138	11	b138
139	18	b139
140	25	b140
141	32	b141
142	39	b142
143	-4	b143
144	3	b144
145	10	b145
146	17	b146
147	24	b147
148	31	b148
149	38	b149
150	-5	b150
151	2	b151
152	9	b152
153	16	\N
154	23	b154
155	30	b155
156	37	b156
157	44	b157
158	1	b158
159	8	b159
160	15	b160
161	22	b161
162	29	b162
163	36	b163
164	43	b164
165	0	b165
166	7	b166
167	14	b167
168	21	b168
169	28	b169
170	35	\N
171	42	b171
172	-1	b172
173	6	b173
174	13	b174
175	20	b175
176	27	b176
177	34	b177
178	41	b178
179	-2	b179
180	5	b180
181	12	b181
182	19	b182
183	26	b183
184	33	b184
185	40	b185
186	-3	b186
187	4	\N
188	11	b188
189	18	b189
190	25	b190
191	32	b191
192	39	b192
193	-4	b193
194	3	b194
195	10	b195
196	17	b196
197	24	b197
198	31	b198
199	38	b199
200	-5	b200
201	2	b201
202	9	b202
203	16	b203
204	23	\N
205	30	b205
206	37	b206
207	44	b207
208	1	b208
209	8	b209
210	15	b210
211	22	b211
212	29	b212
213	36	b213
214	43	b214
215	0	b215
216	7	b216
217	14	b217
218	21	b218
219	28	b219
220	35	b220
221	42	\N
222	-1	b222
223	6	b223
224	13	b224
225	20	b225
226	27	b226
227	34	b227
228	41	b228
229	-2	b229
230	5	b230
231	12	b231
232	19	b232
233	26	b233
234	33	b234
235	40	b235
236	-3	b236
237	4	b237
238	11	\N
239	18	b239
240	25	b240
241	32	b241
242	39	b242
243	-4	b243
244	3	b244
245	10	b245
246	17	b246
247	24	b247
248	31	b248
249	38	b249
250	-5	b250
251	2	b251
252	9	b252
253	16	b253
254	23	b254
255	30	\N
256	37	b256
257	44	b257
258	1	b258
259	8	b259
260	15	b260
261	22	b261
262	29	b262
263	36	b263
264	43	b264
265	0	b265
266	7	b266
267	14	b267
268	21	b268
269	28	b269
270	35	b270
271	42	b271
272	-1	\N
273	6	b273
274	13	b274
275	20	b275
276	27	b276
277	34	b277
278	41	b278
279	-2	b279
280	5	b280
281	12	b281
282	19	b282
283	26	b283
284	33	b284
285	40	b285
286	-3	b286
287	4	b287
288	11	b288
289	18	\N
290	25	b290
291	32	b291
292	39	b292
293	-4	b293
294	3	b294
295	10	b295
296	17	b296
297	24	b297
298	31	b298
299	38	b299
300	-5	b300
301	2	b301
302	9	b302
303	16	b303
304	23	b304
305	30	b305
306	37	\N
307	44	b307
308	1	b308
309	8	b309
310	15	b310
311	22	b311
312	29	b312
313	36	b313
314	43	b314
315	0	b315
316	7	b316
317	14	b317
318	21	b318
319	28	b319
320	35	b320
321	42	b321
322	-1	b322
323	6	\N
324	13	b324
325	20	b325
326	27	b326
327	34	b327
328	41	b328
329	-2	b329
330	5	b330
331	12	b331
332	19	b332
333	26	b333
334	33	b334
335	40	b335
336	-3	b336
337	4	b337
338	11	b338
339	18	b339
340	25	\N
341	32	b341
342	39	b342
343	-4	b343
344	3	b344
345	10	b345
346	17	b346
347	24	b347
348	31	b348
349	38	b349
350	-5	b350
351	2	b351
352	9	b352
353	16	b353
354	23	b354
355	30	b355
356	37	b356
357	44	\N
358	1	b358
359	8	b359
360	15	b360
361	22	b361
362	29	b362
363	36	b363
364	43	b364
365	0	b365
366	7	b366
367	14	b367
368	21	b368
369	28	b369
370	35	b370
371	42	b371
372	-1	b372
373	6	b373
374	13	\N
375	20	b375
376	27	b376
377	34	b377
378	41	b378
379	-2	b379
380	5	b380
381	12	b381
382	19	b382
383	26	b383
384	33	b384
385	40	b385
386	-3	b386
387	4	b387
388	11	b388
389	18	b389
390	25	b390
391	32	\N
392	39	b392
393	-4	b393
394	3	b394
395	10	b395
396	17	b396
397	24	b397
398	31	b398
399	38	b399
400	-5	b400
401	2	b401
402	9	b402
403	16	b403
404	23	b404
405	30	b405
406	37	b406
407	44	b407
408	1	\N
409	8	b409
410	15	b410
411	22	b411
412	29	b412
413	36	b413
414	43	b414
415	0	b415
416	7	b416
417	14	b417
418	21	b418
419	28	b419
420	35	b420
421	42	b421
422	-1	b422
423	6	b423
424	13	b424
425	20	\N
426	27	b426
427	34	b427
428	41	b428
429	-2	b429
430	5	b430
431	12	b431
432	19	b432
433	26	b433
434	33	b434
435	40	b435
436	-3	b436
437	4	b437
438	11	b438
439	18	b439
440	25	b440
441	32	b441
442	39	\N
443	-4	b443
444	3	b444
445	10	b445
446	17	b446
447	24	b447
448	31	b448
449	38	b449
450	-5	b450
451	2	b451
452	9	b452
453	16	b453
454	23	b454
455	30	b455
456	37	b456
457	44	b457
458	1	b458
459	8	\N
460	15	b460
461	22	b461
462	29	b462
463	36	b463
464	43	b464
465	0	b465
466	7	b466
467	14	b467
468	21	b468
469	28	b469
470	35	b470
471	42	b471
472	-1	b472
473	6	b473
474	13	b474
475	20	b475
476	27	\N
477	34	b477
478	41	b478
479	-2	b479
480	5	b480
481	12	b481
482	19	b482
483	26	b483
484	33	b484
485	40	b485
486	-3	b486
487	4	b487
488	11	b488
489	18	b489
490	25	b490
491	32	b491
492	39	b492
493	-4	\N
494	3	b494
495	10	b495
496	17	b496
497	24	b497
498	31	b498
499	38	b499
500	-5	b500
501	2	b501
502	9	b502
503	16	b503
504	23	b504
505	30	b505
506	37	b506
507	44	b507
508	1	b508
509	8	b509
510	15	\N
511	22	b511
512	29	b512
513	36	b513
514	43	b514
515	0	b515
516	7	b516
517	14	b517
518	21	b518
519	28	b519
520	35	b520
521	42	b521
522	-1	b522
523	6	b523
524	13	b524
525	20	b525
526	27	b526
527	34	\N
528	41	b528
529	-2	b529
530	5	b530
531	12	b531
532	19	b532
533	26	b533
534	33	b534
535	40	b535
536	-3	b536
537	4	b537
538	11	b538
539	18	b539
540	25	b540
541	32	b541
542	39	b542
543	-4	b543
544	3	\N
545	10	b545
546	17	b546
547	24	b547
548	31	b548
549	38	b549
550	-5	b550
551	2	b551
552	9	b552
553	16	b553
554	23	b554
555	30	b555
556	37	b556
557	44	b557
558	1	b558
559	8	b559
560	15	b560
561	22	\N
562	29	b562
563	36	b563
564	43	b564
565	0	b565
566	7	b566
567	14	b567
568	21	b568
569	28	b569
570	35	b570
571	42	b571
572	-1	b572
573	6	b573
574	13	b574
575	20	b575
576	27	b576
577	34	b577
578	41	\N
579	-2	b579
580	5	b580
581	12	b581
582	19	b582
583	26	b583
584	33	b584
585	40	b585
586	-3	b586
587	4	b587
588	11	b588
589	18	b589
590	25	b590
591	32	b591
592	39	b592
593	-4	b593
594	3	b594
595	10	\N
596	17	b596
597	24	b597
598	31	b598
599	38	b599
600	-5	b600
601	2	b601
602	9	b602
603	16	b603
604	23	b604
605	30	b605
606	37	b606
607	44	b607
608	1	b608
609	8	b609
610	15	b610
611	22	b611
612	29	\N
613	36	b613
614	43	b614
615	0	b615
616	7	b616
617	14	b617
618	21	b618
619	28	b619
620	35	b620
621	42	b621
622	-1	b622
623	6	b623
624	13	b624
625	20	b625
626	27	b626
627	34	b627
628	41	b628
629	-2	\N
630	5	b630
631	12	b631
632	19	b632
633	26	b633
634	33	b634
635	40	b635
636	-3	b636
637	4	b637
638	11	b638
639	18	b639
640	25	b640
641	32	b641
642	39	b642
643	-4	b643
644	3	b644
645	10	b645
646	17	\N
647	24	b647
648	31	b648
649	38	b649
650	-5	b650
651	2	b651
652	9	b652
653	16	b653
654	23	b654
655	30	b655
656	37	b656
657	44	b657
658	1	b658
659	8	b659
660	15	b660
661	22	b661
662	29	b662
663	36	\N
664	43	b664
665	0	b665
666	7	b666
667	14	b667
668	21	b668
669	28	b669
670	35	b670
671	42	b671
672	-1	b672
673	6	b673
674	13	b674
675	20	b675
676	27	b676
677	34	b677
678	41	b678
679	-2	b679
680	5	\N
681	12	b681
682	19	b682
683	26	b683
684	33	b684
685	40	b685
686	-3	b686
687	4	b687
688	11	b688
689	18	b689
690	25	b690
691	32	b691
692	39	b692
693	-4	b693
694	3	b694
695	10	b695
696	17	b696
697	24	\N
698	31	b698
699	38	b699
700	-5	b700
701	2	b701
702	9	b702
703	16	b703
704	23	b704
705	30	b705
706	37	b706
707	44	b707
708	1	b708
709	8	b709
710	15	b710
711	22	b711
712	29	b712
713	36	b713
714	43	\N
715	0	b715
716	7	b716
717	14	b717
718	21	b718
719	28	b719
720	35	b720
721	42	b721
722	-1	b722
723	6	b723
724	13	b724
725	20	b725
726	27	b726
727	34	b727
728	41	b728
729	-2	b729
730	5	b730
731	12	\N
732	19	b732
733	26	b733
734	33	b734
735	40	b735
736	-3	b736
737	4	b737
738	11	b738
739	18	b739
740	25	b740
741	32	b741
742	39	b742
743	-4	b743
744	3	b744
745	10	b745
746	17	b746
747	24	b747
748	31	\N
749	38	b749
750	-5	b750
751	2	b751
752	9	b752
753	16	b753
754	23	b754
755	30	b755
756	37	b756
757	44	b757
758	1	b758
759	8	b759
760	15	b760
761	22	b761
762	29	b762
763	36	b763
764	43	b764
765	0	\N
766	7	b766
767	14	b767
768	21	b768
769	28	b769
770	35	b770
771	42	b771
772	-1	b772
773	6	b773
774	13	b774
775	20	b775
776	27	b776
777	34	b777
778	41	b778
779	-2	b779
780	5	b780
781	12	b781
782	19	\N
783	26	b783
784	33	b784
785	40	b785
786	-3	b786
787	4	b787
788	11	b788
789	18	b789
790	25	b790
791	32	b791
792	39	b792
793	-4	b793
794	3	b794
795	10	b795
796	17	b796
797	24	b797
798	31	b798
799	38	\N
800	-5	b800
801	2	b801
802	9	b802
803	16	b803
804	23	b804
805	30	b805
806	37	b806
807	44	b807
808	1	b808
809	8	b809
810	15	b810
811	22	b811
812	29	b812
813	36	b813
814	43	b814
815	0	b815
816	7	\N
817	14	b817
818	21	b818
819	28	b819
820	35	b820
821	42	b821
822	-1	b822
823	6	b823
824	13	b824
825	20	b825
826	27	b826
827	34	b827
828	41	b828
829	-2	b829
830	5	b830
831	12	b831
832	19	b832
833	26	\N
834	33	b834
835	40	b835
836	-3	b836
837	4	b837
838	11	b838
839	18	b839
840	25	b840
841	32	b841
842	39	b842
843	-4	b843
844	3	b844
845	10	b845
846	17	b846
847	24	b847
848	31	b848
849	38	b849
850	-5	\N
851	2	b851
852	9	b852
853	16	b853
854	23	b854
855	30	b855
856	37	b856
857	44	b857
858	1	b858
859	8	b859
860	15	b860
861	22	b861
862	29	b862
863	36	b863
864	43	b864
865	0	b865
866	7	b866
867	14	\N
868	21	b868
869	28	b869
870	35	b870
871	42	b871
872	-1	b872
873	6	b873
874	13	b874
875	20	b875
876	27	b876
877	34	b877
878	41	b878
879	-2	b879
880	5	b880
881	12	b881
882	19	b882
883	26	b883
884	33	\N
885	40	b885
886	-3	b886
887	4	b887
888	11	b888
889	18	b889
890	25	b890
891	32	b891
892	39	b892
893	-4	b893
894	3	b894
895	10	b895
896	17	b896
897	24	b897
898	31	b898
899	38	b899
900	-5	b900
901	2	\N
902	9	b902
903	16	b903
904	23	b904
905	30	b905
906	37	b906
907	44	b907
908	1	b908
909	8	b909
910	15	b910
911	22	b911
912	29	b912
913	36	b913
914	43	b914
915	0	b915
916	7	b916
917	14	b917
918	21	\N
919	28	b919
\.

\s2& INSERT INTO r9c4248 (id, a, b) VALUES (130, 0, 'w130');

COMMIT;

\s2.;

DROP TABLE r9c4248;

VACUUM r9s;

VACUUM r9s;

BEGIN;

DECLARE r9cur SCROLL CURSOR FOR SELECT id, v FROM r9s WHERE id BETWEEN 2769 AND 3269 ORDER BY id ;

FETCH 3 FROM r9cur;

FETCH LAST FROM r9cur;

FETCH PRIOR FROM r9cur;

FETCH ABSOLUTE 2 FROM r9cur;

FETCH BACKWARD 3 FROM r9cur;

FETCH RELATIVE -1 FROM r9cur;

CLOSE r9cur;

COMMIT;

UPDATE r7d SET ps[3] = 4 WHERE id = 1 RETURNING ps;

CREATE TABLE r9c94879 (id int PRIMARY KEY, a int, b text) WITH (autovacuum_enabled = off);

CREATE TABLE r9c94879log (n serial, op text, id int, b text);

CREATE FUNCTION r9c94879fb() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN IF NEW.a < -3 THEN RETURN NULL; END IF; IF NEW.a = 13 THEN RAISE EXCEPTION 'r9 bad %', NEW.id; END IF; NEW.b := upper(NEW.b); RETURN NEW; END $f$;

CREATE FUNCTION r9c94879fa() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c94879log (op, id, b) VALUES (TG_WHEN || ' ' || TG_LEVEL, NEW.id, NEW.b); RETURN NULL; END $f$;

CREATE FUNCTION r9c94879fs() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c94879log (op, id, b) SELECT 'stmt', count(*), max(b) FROM nt; RETURN NULL; END $f$;

CREATE TRIGGER r9c94879a AFTER INSERT ON r9c94879 FOR EACH ROW EXECUTE FUNCTION r9c94879fa();

BEGIN;

COPY r9c94879 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
7	44	b7
8	1	b8
9	8	b9
10	15	b10
11	22	b11
12	29	b12
13	36	b13
14	43	b14
15	0	b15
16	7	b16
17	14	\N
18	21	b18
19	28	b19
20	35	b20
21	42	b21
22	-1	b22
23	6	b23
24	13	b24
25	20	b25
26	27	b26
27	34	b27
28	41	b28
29	-2	b29
30	5	b30
31	12	b31
32	19	b32
33	26	b33
34	33	\N
35	40	b35
36	-3	b36
37	4	b37
38	11	b38
39	18	b39
40	25	b40
41	32	b41
42	39	b42
43	-4	b43
44	3	b44
45	10	b45
46	17	b46
47	24	b47
48	31	b48
49	38	b49
50	-5	b50
51	2	\N
52	9	b52
53	16	b53
54	23	b54
55	30	b55
56	37	b56
57	44	b57
58	1	b58
59	8	b59
60	15	b60
61	22	b61
62	29	b62
63	36	b63
64	43	b64
65	0	b65
66	7	b66
67	14	b67
68	21	\N
69	28	b69
70	35	b70
71	42	b71
72	-1	b72
73	6	b73
74	13	b74
75	20	b75
76	27	b76
77	34	b77
78	41	b78
79	-2	b79
80	5	b80
81	12	b81
82	19	b82
83	26	b83
84	33	b84
85	40	\N
86	-3	b86
87	4	b87
88	11	b88
89	18	b89
90	25	b90
91	32	b91
92	39	b92
93	-4	b93
94	3	b94
95	10	b95
96	17	b96
97	24	b97
98	31	b98
99	38	b99
100	-5	b100
101	2	b101
102	9	\N
103	16	b103
104	23	b104
105	30	b105
106	37	b106
107	44	b107
108	1	b108
109	8	b109
110	15	b110
111	22	b111
112	29	b112
113	36	b113
114	43	b114
115	0	b115
116	7	b116
117	14	b117
118	21	b118
119	28	\N
120	35	b120
121	42	b121
122	-1	b122
123	6	b123
124	13	b124
125	20	b125
126	27	b126
127	34	b127
128	41	b128
129	-2	b129
130	5	b130
131	12	b131
132	19	b132
133	26	b133
134	33	b134
135	40	b135
136	-3	\N
137	4	b137
138	11	b138
139	18	b139
140	25	b140
141	32	b141
142	39	b142
143	-4	b143
144	3	b144
145	10	b145
146	17	b146
147	24	b147
148	31	b148
149	38	b149
150	-5	b150
151	2	b151
152	9	b152
153	16	\N
154	23	b154
155	30	b155
156	37	b156
157	44	b157
158	1	b158
159	8	b159
160	15	b160
161	22	b161
162	29	b162
163	36	b163
164	43	b164
165	0	b165
166	7	b166
167	14	b167
168	21	b168
169	28	b169
170	35	\N
171	42	b171
172	-1	b172
173	6	b173
174	13	b174
175	20	b175
176	27	b176
177	34	b177
178	41	b178
179	-2	b179
180	5	b180
181	12	b181
182	19	b182
183	26	b183
184	33	b184
185	40	b185
186	-3	b186
187	4	\N
188	11	b188
189	18	b189
190	25	b190
191	32	b191
192	39	b192
193	-4	b193
194	3	b194
195	10	b195
196	17	b196
197	24	b197
198	31	b198
199	38	b199
200	-5	b200
201	2	b201
202	9	b202
203	16	b203
204	23	\N
205	30	b205
206	37	b206
207	44	b207
208	1	b208
209	8	b209
210	15	b210
211	22	b211
212	29	b212
213	36	b213
214	43	b214
215	0	b215
216	7	b216
217	14	b217
218	21	b218
219	28	b219
220	35	b220
221	42	\N
222	-1	b222
223	6	b223
224	13	b224
225	20	b225
226	27	b226
227	34	b227
228	41	b228
229	-2	b229
230	5	b230
231	12	b231
232	19	b232
233	26	b233
234	33	b234
235	40	b235
236	-3	b236
237	4	b237
238	11	\N
239	18	b239
240	25	b240
241	32	b241
242	39	b242
243	-4	b243
244	3	b244
245	10	b245
246	17	b246
247	24	b247
248	31	b248
249	38	b249
250	-5	b250
251	2	b251
252	9	b252
253	16	b253
254	23	b254
255	30	\N
256	37	b256
257	44	b257
258	1	b258
259	8	b259
260	15	b260
261	22	b261
262	29	b262
263	36	b263
264	43	b264
265	0	b265
266	7	b266
267	14	b267
268	21	b268
269	28	b269
270	35	b270
271	42	b271
272	-1	\N
273	6	b273
274	13	b274
275	20	b275
276	27	b276
277	34	b277
278	41	b278
279	-2	b279
280	5	b280
281	12	b281
282	19	b282
283	26	b283
284	33	b284
285	40	b285
286	-3	b286
287	4	b287
288	11	b288
289	18	\N
290	25	b290
291	32	b291
292	39	b292
293	-4	b293
294	3	b294
295	10	b295
296	17	b296
297	24	b297
298	31	b298
299	38	b299
300	-5	b300
301	2	b301
302	9	b302
303	16	b303
304	23	b304
305	30	b305
306	37	\N
307	44	b307
308	1	b308
309	8	b309
310	15	b310
311	22	b311
312	29	b312
313	36	b313
314	43	b314
315	0	b315
316	7	b316
317	14	b317
318	21	b318
319	28	b319
320	35	b320
321	42	b321
322	-1	b322
323	6	\N
324	13	b324
325	20	b325
326	27	b326
327	34	b327
328	41	b328
329	-2	b329
330	5	b330
331	12	b331
332	19	b332
333	26	b333
334	33	b334
335	40	b335
336	-3	b336
337	4	b337
338	11	b338
339	18	b339
340	25	\N
341	32	b341
342	39	b342
343	-4	b343
344	3	b344
345	10	b345
346	17	b346
347	24	b347
348	31	b348
349	38	b349
350	-5	b350
351	2	b351
352	9	b352
353	16	b353
354	23	b354
355	30	b355
356	37	b356
357	44	\N
358	1	b358
359	8	b359
360	15	b360
361	22	b361
362	29	b362
363	36	b363
364	43	b364
365	0	b365
366	7	b366
367	14	b367
368	21	b368
369	28	b369
370	35	b370
371	42	b371
372	-1	b372
373	6	b373
374	13	\N
375	20	b375
376	27	b376
377	34	b377
378	41	b378
379	-2	b379
380	5	b380
381	12	b381
382	19	b382
383	26	b383
384	33	b384
385	40	b385
386	-3	b386
387	4	b387
388	11	b388
389	18	b389
390	25	b390
391	32	\N
392	39	b392
393	-4	b393
394	3	b394
395	10	b395
396	17	b396
397	24	b397
398	31	b398
399	38	b399
400	-5	b400
401	2	b401
402	9	b402
403	16	b403
404	23	b404
405	30	b405
406	37	b406
407	44	b407
408	1	\N
409	8	b409
410	15	b410
411	22	b411
412	29	b412
413	36	b413
414	43	b414
415	0	b415
416	7	b416
417	14	b417
418	21	b418
419	28	b419
420	35	b420
421	42	b421
422	-1	b422
423	6	b423
424	13	b424
425	20	\N
426	27	b426
427	34	b427
428	41	b428
429	-2	b429
430	5	b430
431	12	b431
432	19	b432
433	26	b433
434	33	b434
435	40	b435
436	-3	b436
437	4	b437
438	11	b438
439	18	b439
440	25	b440
441	32	b441
442	39	\N
443	-4	b443
444	3	b444
445	10	b445
446	17	b446
447	24	b447
448	31	b448
449	38	b449
450	-5	b450
451	2	b451
452	9	b452
453	16	b453
454	23	b454
455	30	b455
456	37	b456
457	44	b457
458	1	b458
459	8	\N
460	15	b460
461	22	b461
462	29	b462
463	36	b463
464	43	b464
465	0	b465
466	7	b466
467	14	b467
468	21	b468
469	28	b469
470	35	b470
471	42	b471
472	-1	b472
473	6	b473
474	13	b474
475	20	b475
476	27	\N
477	34	b477
478	41	b478
479	-2	b479
480	5	b480
481	12	b481
482	19	b482
483	26	b483
484	33	b484
485	40	b485
486	-3	b486
487	4	b487
488	11	b488
489	18	b489
490	25	b490
491	32	b491
492	39	b492
493	-4	\N
494	3	b494
495	10	b495
496	17	b496
497	24	b497
498	31	b498
499	38	b499
500	-5	b500
501	2	b501
502	9	b502
503	16	b503
504	23	b504
505	30	b505
506	37	b506
507	44	b507
508	1	b508
509	8	b509
510	15	\N
511	22	b511
512	29	b512
513	36	b513
514	43	b514
515	0	b515
516	7	b516
517	14	b517
518	21	b518
519	28	b519
520	35	b520
521	42	b521
522	-1	b522
523	6	b523
524	13	b524
525	20	b525
526	27	b526
527	34	\N
528	41	b528
529	-2	b529
530	5	b530
531	12	b531
532	19	b532
533	26	b533
534	33	b534
535	40	b535
536	-3	b536
537	4	b537
538	11	b538
539	18	b539
540	25	b540
541	32	b541
542	39	b542
543	-4	b543
544	3	\N
545	10	b545
546	17	b546
547	24	b547
548	31	b548
549	38	b549
550	-5	b550
551	2	b551
552	9	b552
553	16	b553
554	23	b554
555	30	b555
556	37	b556
557	44	b557
558	1	b558
559	8	b559
560	15	b560
561	22	\N
562	29	b562
563	36	b563
564	43	b564
565	0	b565
566	7	b566
567	14	b567
568	21	b568
569	28	b569
570	35	b570
571	42	b571
572	-1	b572
573	6	b573
574	13	b574
575	20	b575
576	27	b576
577	34	b577
578	41	\N
579	-2	b579
580	5	b580
581	12	b581
582	19	b582
583	26	b583
584	33	b584
585	40	b585
586	-3	b586
587	4	b587
588	11	b588
589	18	b589
590	25	b590
591	32	b591
592	39	b592
593	-4	b593
594	3	b594
595	10	\N
596	17	b596
597	24	b597
598	31	b598
599	38	b599
600	-5	b600
601	2	b601
602	9	b602
603	16	b603
604	23	b604
605	30	b605
606	37	b606
607	44	b607
608	1	b608
609	8	b609
610	15	b610
611	22	b611
612	29	\N
613	36	b613
614	43	b614
615	0	b615
616	7	b616
617	14	b617
618	21	b618
619	28	b619
620	35	b620
621	42	b621
622	-1	b622
623	6	b623
624	13	b624
625	20	b625
626	27	b626
627	34	b627
628	41	b628
629	-2	\N
630	5	b630
631	12	b631
632	19	b632
633	26	b633
634	33	b634
635	40	b635
636	-3	b636
637	4	b637
638	11	b638
639	18	b639
640	25	b640
641	32	b641
642	39	b642
643	-4	b643
644	3	b644
645	10	b645
646	17	\N
647	24	b647
648	31	b648
649	38	b649
650	-5	b650
651	2	b651
652	9	b652
653	16	b653
654	23	b654
655	30	b655
656	37	b656
657	44	b657
658	1	b658
659	8	b659
660	15	b660
661	22	b661
662	29	b662
663	36	\N
664	43	b664
665	0	b665
666	7	b666
667	14	b667
668	21	b668
669	28	b669
670	35	b670
671	42	b671
672	-1	b672
673	6	b673
674	13	b674
675	20	b675
676	27	b676
677	34	b677
678	41	b678
679	-2	b679
680	5	\N
681	12	b681
682	19	b682
683	26	b683
684	33	b684
685	40	b685
686	-3	b686
687	4	b687
688	11	b688
689	18	b689
690	25	b690
691	32	b691
692	39	b692
693	-4	b693
694	3	b694
695	10	b695
696	17	b696
697	24	\N
698	31	b698
699	38	b699
700	-5	b700
701	2	b701
702	9	b702
703	16	b703
704	23	b704
705	30	b705
706	37	b706
707	44	b707
708	1	b708
709	8	b709
710	15	b710
711	22	b711
712	29	b712
713	36	b713
714	43	\N
715	0	b715
716	7	b716
717	14	b717
718	21	b718
719	28	b719
720	35	b720
721	42	b721
722	-1	b722
723	6	b723
724	13	b724
725	20	b725
726	27	b726
727	34	b727
728	41	b728
729	-2	b729
730	5	b730
731	12	\N
732	19	b732
733	26	b733
734	33	b734
735	40	b735
736	-3	b736
737	4	b737
738	11	b738
739	18	b739
740	25	b740
741	32	b741
742	39	b742
743	-4	b743
744	3	b744
745	10	b745
746	17	b746
747	24	b747
748	31	\N
749	38	b749
750	-5	b750
751	2	b751
752	9	b752
753	16	b753
754	23	b754
755	30	b755
756	37	b756
757	44	b757
758	1	b758
759	8	b759
760	15	b760
761	22	b761
762	29	b762
763	36	b763
764	43	b764
765	0	\N
766	7	b766
767	14	b767
768	21	b768
769	28	b769
770	35	b770
771	42	b771
772	-1	b772
773	6	b773
774	13	b774
775	20	b775
776	27	b776
777	34	b777
778	41	b778
779	-2	b779
780	5	b780
781	12	b781
782	19	\N
783	26	b783
784	33	b784
785	40	b785
786	-3	b786
787	4	b787
788	11	b788
789	18	b789
790	25	b790
791	32	b791
792	39	b792
793	-4	b793
794	3	b794
795	10	b795
796	17	b796
797	24	b797
798	31	b798
799	38	\N
800	-5	b800
801	2	b801
802	9	b802
803	16	b803
804	23	b804
805	30	b805
806	37	b806
807	44	b807
808	1	b808
809	8	b809
810	15	b810
811	22	b811
812	29	b812
813	36	b813
814	43	b814
815	0	b815
816	7	\N
817	14	b817
818	21	b818
819	28	b819
820	35	b820
821	42	b821
822	-1	b822
823	6	b823
824	13	b824
825	20	b825
826	27	b826
827	34	b827
828	41	b828
829	-2	b829
830	5	b830
831	12	b831
832	19	b832
833	26	\N
834	33	b834
835	40	b835
836	-3	b836
837	4	b837
838	11	b838
839	18	b839
840	25	b840
841	32	b841
842	39	b842
843	-4	b843
844	3	b844
845	10	b845
846	17	b846
847	24	b847
848	31	b848
849	38	b849
850	-5	\N
851	2	b851
852	9	b852
853	16	b853
854	23	b854
855	30	b855
856	37	b856
857	44	b857
858	1	b858
859	8	b859
860	15	b860
861	22	b861
862	29	b862
863	36	b863
864	43	b864
865	0	b865
866	7	b866
867	14	\N
868	21	b868
869	28	b869
870	35	b870
871	42	b871
872	-1	b872
873	6	b873
874	13	b874
875	20	b875
876	27	b876
877	34	b877
878	41	b878
879	-2	b879
880	5	b880
881	12	b881
882	19	b882
883	26	b883
884	33	\N
885	40	b885
886	-3	b886
887	4	b887
888	11	b888
889	18	b889
890	25	b890
891	32	b891
892	39	b892
893	-4	b893
894	3	b894
895	10	b895
896	17	b896
897	24	b897
898	31	b898
899	38	b899
900	-5	b900
901	2	\N
902	9	b902
903	16	b903
904	23	b904
905	30	b905
906	37	b906
907	44	b907
908	1	b908
909	8	b909
910	15	b910
911	22	b911
912	29	b912
913	36	b913
914	43	b914
915	0	b915
916	7	b916
917	14	b917
918	21	\N
919	28	b919
920	35	b920
921	42	b921
922	-1	b922
923	6	b923
924	13	b924
925	20	b925
926	27	b926
927	34	b927
928	41	b928
929	-2	b929
930	5	b930
931	12	b931
932	19	b932
933	26	b933
934	33	b934
935	40	\N
936	-3	b936
937	4	b937
938	11	b938
939	18	b939
940	25	b940
941	32	b941
942	39	b942
943	-4	b943
944	3	b944
945	10	b945
946	17	b946
947	24	b947
948	31	b948
949	38	b949
950	-5	b950
951	2	b951
952	9	\N
953	16	b953
954	23	b954
955	30	b955
956	37	b956
957	44	b957
958	1	b958
959	8	b959
960	15	b960
961	22	b961
962	29	b962
963	36	b963
964	43	b964
965	0	b965
966	7	b966
967	14	b967
968	21	b968
969	28	\N
970	35	b970
971	42	b971
972	-1	b972
973	6	b973
974	13	b974
975	20	b975
976	27	b976
977	34	b977
978	41	b978
979	-2	b979
980	5	b980
981	12	b981
982	19	b982
983	26	b983
984	33	b984
985	40	b985
986	-3	\N
987	4	b987
988	11	b988
989	18	b989
990	25	b990
991	32	b991
992	39	b992
993	-4	b993
994	3	b994
995	10	b995
996	17	b996
997	24	b997
998	31	b998
999	38	b999
1000	-5	b1000
1001	2	b1001
1002	9	b1002
1003	16	\N
1004	23	b1004
1005	30	b1005
1006	37	b1006
1007	44	b1007
1008	1	b1008
1009	8	b1009
1010	15	b1010
1011	22	b1011
1012	29	b1012
1013	36	b1013
1014	43	b1014
1015	0	b1015
1016	7	b1016
1017	14	b1017
1018	21	b1018
1019	28	b1019
1020	35	\N
1021	42	b1021
1022	-1	b1022
1023	6	b1023
1024	13	b1024
1025	20	b1025
1026	27	b1026
1027	34	b1027
1028	41	b1028
1029	-2	b1029
1030	5	b1030
1031	12	b1031
1032	19	b1032
1033	26	b1033
1034	33	b1034
1035	40	b1035
1036	-3	b1036
1037	4	\N
1038	11	b1038
1039	18	b1039
1040	25	b1040
1041	32	b1041
1042	39	b1042
1043	-4	b1043
1044	3	b1044
1045	10	b1045
1046	17	b1046
1047	24	b1047
1048	31	b1048
1049	38	b1049
1050	-5	b1050
1051	2	b1051
1052	9	b1052
1053	16	b1053
1054	23	\N
1055	30	b1055
1056	37	b1056
1057	44	b1057
1058	1	b1058
1059	8	b1059
1060	15	b1060
1061	22	b1061
1062	29	b1062
\.

\s2& INSERT INTO r9c94879 (id, a, b) VALUES (241, 0, 'w241') ON CONFLICT DO NOTHING;

COMMIT;

\s2.;

DROP TABLE r9c94879log;

DROP TABLE r9c94879;

DROP FUNCTION r9c94879fb(), r9c94879fa(), r9c94879fs();

SET plan_cache_mode = force_custom_plan;

PREPARE r9p99806 (int) AS SELECT CASE WHEN $1 > 1 THEN 'many' ELSE (SELECT max(t) FROM r9s WHERE g = $1) END;

EXECUTE r9p99806(99);

EXECUTE r9p99806(1);

EXECUTE r9p99806(1);

EXPLAIN (COSTS OFF) EXECUTE r9p99806(1);

EXECUTE r9p99806(1);

EXPLAIN (COSTS OFF) EXECUTE r9p99806(1);

EXECUTE r9p99806(1);

EXECUTE r9p99806(1);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p99806';

DEALLOCATE r9p99806;

RESET plan_cache_mode;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = auto; FOR i IN 0..13 LOOP s := s + coalesce((SELECT count(*) FROM r9s WHERE g = i % 9 AND v > 10 / (i % 6)), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

CREATE TABLE r9c11858 (id int PRIMARY KEY, a int, b text) WITH (autovacuum_enabled = off);

CREATE TABLE r9c11858log (n serial, op text, id int, b text);

CREATE FUNCTION r9c11858fb() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN IF NEW.a < -3 THEN RETURN NULL; END IF; IF NEW.a = 13 THEN RAISE EXCEPTION 'r9 bad %', NEW.id; END IF; NEW.b := upper(NEW.b); RETURN NEW; END $f$;

CREATE FUNCTION r9c11858fa() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c11858log (op, id, b) VALUES (TG_WHEN || ' ' || TG_LEVEL, NEW.id, NEW.b); RETURN NULL; END $f$;

CREATE FUNCTION r9c11858fs() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c11858log (op, id, b) SELECT 'stmt', count(*), max(b) FROM nt; RETURN NULL; END $f$;

CREATE TRIGGER r9c11858b BEFORE INSERT ON r9c11858 FOR EACH ROW EXECUTE FUNCTION r9c11858fb();

CREATE TRIGGER r9c11858a AFTER INSERT ON r9c11858 FOR EACH ROW EXECUTE FUNCTION r9c11858fa();

BEGIN;

COPY r9c11858 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b5
6	37	b6
7	44	b7
8	1	b8
9	8	b9
10	15	b10
11	22	b11
12	29	b12
13	36	b13
14	43	b14
15	0	b15
16	7	b16
17	14	\N
18	21	b18
19	28	b19
20	35	b20
21	42	b21
22	-1	b22
23	6	b23
24	13	b24
25	20	b25
26	27	b26
27	34	b27
28	41	b28
29	-2	b29
30	5	b30
31	12	b31
32	19	b32
33	26	b33
34	33	\N
35	40	b35
36	-3	b36
37	4	b37
38	11	b38
39	18	b39
40	25	b40
41	32	b41
42	39	b42
43	-4	b43
44	3	b44
45	10	b45
46	17	b46
47	24	b47
48	31	b48
49	38	b49
50	-5	b50
51	2	\N
52	9	b52
53	16	b53
54	23	b54
55	30	b55
56	37	b56
57	44	b57
58	1	b58
59	8	b59
60	15	b60
61	22	b61
62	29	b62
63	36	b63
64	43	b64
65	0	b65
66	7	b66
67	14	b67
68	21	\N
69	28	b69
70	35	b70
71	42	b71
72	-1	b72
73	6	b73
74	13	b74
75	20	b75
76	27	b76
77	34	b77
78	41	b78
79	-2	b79
80	5	b80
81	12	b81
82	19	b82
83	26	b83
84	33	b84
85	40	\N
86	-3	b86
87	4	b87
88	11	b88
89	18	b89
90	25	b90
91	32	b91
92	39	b92
93	-4	b93
94	3	b94
95	10	b95
96	17	b96
97	24	b97
98	31	b98
99	38	b99
100	-5	b100
101	2	b101
102	9	\N
103	16	b103
104	23	b104
105	30	b105
106	37	b106
107	44	b107
108	1	b108
109	8	b109
110	15	b110
111	22	b111
112	29	b112
113	36	b113
114	43	b114
115	0	b115
116	7	b116
117	14	b117
118	21	b118
119	28	\N
120	35	b120
121	42	b121
122	-1	b122
123	6	b123
124	13	b124
125	20	b125
126	27	b126
127	34	b127
128	41	b128
129	-2	b129
130	5	b130
131	12	b131
132	19	b132
133	26	b133
134	33	b134
135	40	b135
136	-3	\N
137	4	b137
138	11	b138
139	18	b139
140	25	b140
141	32	b141
142	39	b142
143	-4	b143
144	3	b144
145	10	b145
146	17	b146
147	24	b147
148	31	b148
149	38	b149
150	-5	b150
151	2	b151
152	9	b152
153	16	\N
154	23	b154
155	30	b155
156	37	b156
157	44	b157
158	1	b158
159	8	b159
160	15	b160
161	22	b161
162	29	b162
163	36	b163
164	43	b164
165	0	b165
166	7	b166
167	14	b167
168	21	b168
169	28	b169
170	35	\N
171	42	b171
172	-1	b172
173	6	b173
174	13	b174
175	20	b175
176	27	b176
177	34	b177
178	41	b178
179	-2	b179
180	5	b180
181	12	b181
182	19	b182
183	26	b183
184	33	b184
185	40	b185
186	-3	b186
187	4	\N
188	11	b188
189	18	b189
190	25	b190
191	32	b191
192	39	b192
193	-4	b193
194	3	b194
195	10	b195
196	17	b196
197	24	b197
198	31	b198
199	38	b199
200	-5	b200
201	2	b201
202	9	b202
203	16	b203
204	23	\N
205	30	b205
206	37	b206
207	44	b207
208	1	b208
209	8	b209
210	15	b210
211	22	b211
212	29	b212
213	36	b213
214	43	b214
215	0	b215
216	7	b216
217	14	b217
218	21	b218
219	28	b219
220	35	b220
221	42	\N
222	-1	b222
223	6	b223
224	13	b224
225	20	b225
226	27	b226
227	34	b227
228	41	b228
229	-2	b229
230	5	b230
231	12	b231
232	19	b232
233	26	b233
234	33	b234
235	40	b235
236	-3	b236
237	4	b237
238	11	\N
239	18	b239
240	25	b240
241	32	b241
242	39	b242
243	-4	b243
244	3	b244
245	10	b245
246	17	b246
247	24	b247
248	31	b248
249	38	b249
250	-5	b250
251	2	b251
252	9	b252
253	16	b253
254	23	b254
255	30	\N
256	37	b256
257	44	b257
258	1	b258
259	8	b259
260	15	b260
261	22	b261
262	29	b262
263	36	b263
264	43	b264
265	0	b265
266	7	b266
267	14	b267
268	21	b268
269	28	b269
270	35	b270
271	42	b271
272	-1	\N
273	6	b273
274	13	b274
275	20	b275
276	27	b276
277	34	b277
278	41	b278
279	-2	b279
280	5	b280
281	12	b281
282	19	b282
283	26	b283
284	33	b284
285	40	b285
286	-3	b286
287	4	b287
288	11	b288
289	18	\N
290	25	b290
291	32	b291
292	39	b292
293	-4	b293
294	3	b294
295	10	b295
296	17	b296
297	24	b297
298	31	b298
299	38	b299
300	-5	b300
301	2	b301
302	9	b302
303	16	b303
304	23	b304
305	30	b305
306	37	\N
307	44	b307
308	1	b308
309	8	b309
310	15	b310
311	22	b311
312	29	b312
313	36	b313
314	43	b314
315	0	b315
316	7	b316
317	14	b317
318	21	b318
319	28	b319
320	35	b320
321	42	b321
322	-1	b322
323	6	\N
324	13	b324
325	20	b325
326	27	b326
327	34	b327
328	41	b328
329	-2	b329
330	5	b330
331	12	b331
332	19	b332
333	26	b333
334	33	b334
335	40	b335
336	-3	b336
337	4	b337
338	11	b338
339	18	b339
340	25	\N
341	32	b341
342	39	b342
343	-4	b343
344	3	b344
345	10	b345
346	17	b346
347	24	b347
348	31	b348
349	38	b349
350	-5	b350
351	2	b351
352	9	b352
353	16	b353
354	23	b354
355	30	b355
356	37	b356
357	44	\N
358	1	b358
359	8	b359
360	15	b360
361	22	b361
362	29	b362
363	36	b363
364	43	b364
365	0	b365
366	7	b366
367	14	b367
368	21	b368
369	28	b369
370	35	b370
371	42	b371
372	-1	b372
373	6	b373
374	13	\N
375	20	b375
376	27	b376
377	34	b377
378	41	b378
379	-2	b379
380	5	b380
381	12	b381
382	19	b382
383	26	b383
384	33	b384
385	40	b385
386	-3	b386
387	4	b387
388	11	b388
389	18	b389
390	25	b390
391	32	\N
392	39	b392
393	-4	b393
394	3	b394
395	10	b395
396	17	b396
397	24	b397
398	31	b398
399	38	b399
400	-5	b400
401	2	b401
402	9	b402
403	16	b403
404	23	b404
405	30	b405
406	37	b406
407	44	b407
408	1	\N
409	8	b409
410	15	b410
411	22	b411
412	29	b412
413	36	b413
414	43	b414
415	0	b415
416	7	b416
417	14	b417
418	21	b418
419	28	b419
420	35	b420
421	42	b421
422	-1	b422
423	6	b423
424	13	b424
425	20	\N
426	27	b426
427	34	b427
428	41	b428
429	-2	b429
430	5	b430
431	12	b431
432	19	b432
433	26	b433
434	33	b434
435	40	b435
436	-3	b436
437	4	b437
438	11	b438
439	18	b439
440	25	b440
441	32	b441
442	39	\N
443	-4	b443
444	3	b444
445	10	b445
446	17	b446
447	24	b447
448	31	b448
449	38	b449
450	-5	b450
451	2	b451
452	9	b452
453	16	b453
454	23	b454
455	30	b455
456	37	b456
457	44	b457
458	1	b458
459	8	\N
460	15	b460
461	22	b461
462	29	b462
463	36	b463
464	43	b464
465	0	b465
466	7	b466
467	14	b467
468	21	b468
469	28	b469
470	35	b470
471	42	b471
472	-1	b472
473	6	b473
474	13	b474
475	20	b475
476	27	\N
477	34	b477
478	41	b478
479	-2	b479
480	5	b480
481	12	b481
482	19	b482
483	26	b483
484	33	b484
485	40	b485
486	-3	b486
487	4	b487
488	11	b488
489	18	b489
490	25	b490
491	32	b491
492	39	b492
493	-4	\N
494	3	b494
495	10	b495
496	17	b496
497	24	b497
498	31	b498
499	38	b499
500	-5	b500
501	2	b501
502	9	b502
503	16	b503
504	23	b504
505	30	b505
506	37	b506
507	44	b507
508	1	b508
509	8	b509
510	15	\N
511	22	b511
512	29	b512
513	36	b513
514	43	b514
515	0	b515
516	7	b516
517	14	b517
518	21	b518
519	28	b519
520	35	b520
521	42	b521
522	-1	b522
523	6	b523
524	13	b524
525	20	b525
526	27	b526
527	34	\N
528	41	b528
529	-2	b529
530	5	b530
531	12	b531
532	19	b532
533	26	b533
534	33	b534
535	40	b535
536	-3	b536
537	4	b537
538	11	b538
539	18	b539
540	25	b540
541	32	b541
542	39	b542
543	-4	b543
544	3	\N
545	10	b545
546	17	b546
547	24	b547
548	31	b548
549	38	b549
550	-5	b550
551	2	b551
552	9	b552
553	16	b553
554	23	b554
555	30	b555
556	37	b556
557	44	b557
558	1	b558
559	8	b559
560	15	b560
561	22	\N
562	29	b562
563	36	b563
564	43	b564
565	0	b565
566	7	b566
567	14	b567
568	21	b568
569	28	b569
570	35	b570
571	42	b571
572	-1	b572
573	6	b573
574	13	b574
575	20	b575
576	27	b576
577	34	b577
578	41	\N
579	-2	b579
580	5	b580
581	12	b581
582	19	b582
583	26	b583
584	33	b584
585	40	b585
586	-3	b586
587	4	b587
588	11	b588
589	18	b589
590	25	b590
591	32	b591
592	39	b592
593	-4	b593
594	3	b594
595	10	\N
596	17	b596
597	24	b597
598	31	b598
599	38	b599
600	-5	b600
601	2	b601
602	9	b602
603	16	b603
604	23	b604
605	30	b605
606	37	b606
607	44	b607
608	1	b608
609	8	b609
610	15	b610
611	22	b611
612	29	\N
613	36	b613
614	43	b614
615	0	b615
616	7	b616
617	14	b617
618	21	b618
619	28	b619
620	35	b620
621	42	b621
622	-1	b622
623	6	b623
624	13	b624
625	20	b625
626	27	b626
627	34	b627
628	41	b628
629	-2	\N
630	5	b630
631	12	b631
632	19	b632
633	26	b633
634	33	b634
635	40	b635
636	-3	b636
637	4	b637
638	11	b638
639	18	b639
640	25	b640
641	32	b641
642	39	b642
643	-4	b643
644	3	b644
645	10	b645
646	17	\N
647	24	b647
648	31	b648
649	38	b649
650	-5	b650
651	2	b651
652	9	b652
653	16	b653
654	23	b654
655	30	b655
656	37	b656
657	44	b657
658	1	b658
659	8	b659
660	15	b660
661	22	b661
662	29	b662
663	36	\N
664	43	b664
665	0	b665
666	7	b666
667	14	b667
668	21	b668
669	28	b669
670	35	b670
671	42	b671
672	-1	b672
673	6	b673
674	13	b674
675	20	b675
676	27	b676
677	34	b677
678	41	b678
679	-2	b679
680	5	\N
681	12	b681
682	19	b682
683	26	b683
684	33	b684
685	40	b685
686	-3	b686
687	4	b687
688	11	b688
689	18	b689
690	25	b690
691	32	b691
692	39	b692
693	-4	b693
694	3	b694
695	10	b695
696	17	b696
697	24	\N
698	31	b698
699	38	b699
700	-5	b700
701	2	b701
702	9	b702
703	16	b703
704	23	b704
705	30	b705
706	37	b706
707	44	b707
708	1	b708
709	8	b709
710	15	b710
711	22	b711
712	29	b712
713	36	b713
714	43	\N
715	0	b715
716	7	b716
717	14	b717
718	21	b718
719	28	b719
720	35	b720
721	42	b721
722	-1	b722
723	6	b723
724	13	b724
725	20	b725
726	27	b726
727	34	b727
728	41	b728
729	-2	b729
730	5	b730
731	12	\N
732	19	b732
733	26	b733
734	33	b734
735	40	b735
736	-3	b736
737	4	b737
738	11	b738
739	18	b739
740	25	b740
741	32	b741
742	39	b742
743	-4	b743
744	3	b744
745	10	b745
746	17	b746
747	24	b747
748	31	\N
749	38	b749
750	-5	b750
751	2	b751
752	9	b752
753	16	b753
754	23	b754
755	30	b755
756	37	b756
757	44	b757
758	1	b758
759	8	b759
760	15	b760
761	22	b761
762	29	b762
763	36	b763
764	43	b764
765	0	\N
766	7	b766
767	14	b767
768	21	b768
769	28	b769
770	35	b770
771	42	b771
772	-1	b772
773	6	b773
774	13	b774
775	20	b775
776	27	b776
777	34	b777
778	41	b778
779	-2	b779
780	5	b780
781	12	b781
782	19	\N
783	26	b783
784	33	b784
785	40	b785
786	-3	b786
787	4	b787
788	11	b788
789	18	b789
790	25	b790
791	32	b791
792	39	b792
793	-4	b793
794	3	b794
795	10	b795
796	17	b796
797	24	b797
798	31	b798
799	38	\N
800	-5	b800
801	2	b801
802	9	b802
803	16	b803
804	23	b804
805	30	b805
806	37	b806
807	44	b807
808	1	b808
809	8	b809
810	15	b810
811	22	b811
812	29	b812
813	36	b813
814	43	b814
815	0	b815
816	7	\N
817	14	b817
818	21	b818
819	28	b819
820	35	b820
821	42	b821
822	-1	b822
823	6	b823
824	13	b824
825	20	b825
826	27	b826
827	34	b827
828	41	b828
829	-2	b829
830	5	b830
831	12	b831
832	19	b832
833	26	\N
834	33	b834
835	40	b835
836	-3	b836
837	4	b837
838	11	b838
839	18	b839
840	25	b840
841	32	b841
842	39	b842
843	-4	b843
844	3	b844
845	10	b845
846	17	b846
847	24	b847
848	31	b848
849	38	b849
850	-5	\N
851	2	b851
852	9	b852
853	16	b853
854	23	b854
855	30	b855
856	37	b856
857	44	b857
858	1	b858
859	8	b859
860	15	b860
861	22	b861
862	29	b862
863	36	b863
864	43	b864
865	0	b865
866	7	b866
867	14	\N
868	21	b868
869	28	b869
870	35	b870
871	42	b871
872	-1	b872
873	6	b873
874	13	b874
875	20	b875
876	27	b876
877	34	b877
878	41	b878
879	-2	b879
880	5	b880
881	12	b881
882	19	b882
883	26	b883
884	33	\N
885	40	b885
886	-3	b886
887	4	b887
888	11	b888
889	18	b889
890	25	b890
891	32	b891
892	39	b892
893	-4	b893
894	3	b894
895	10	b895
896	17	b896
897	24	b897
898	31	b898
899	38	b899
900	-5	b900
901	2	\N
902	9	b902
903	16	b903
904	23	b904
905	30	b905
906	37	b906
907	44	b907
908	1	b908
909	8	b909
910	15	b910
911	22	b911
912	29	b912
913	36	b913
914	43	b914
915	0	b915
916	7	b916
917	14	b917
918	21	\N
919	28	b919
920	35	b920
921	42	b921
922	-1	b922
923	6	b923
924	13	b924
925	20	b925
926	27	b926
927	34	b927
928	41	b928
929	-2	b929
930	5	b930
931	12	b931
932	19	b932
933	26	b933
934	33	b934
935	40	b1
936	-3	b936
937	4	b937
938	11	b938
939	18	b939
940	25	b940
941	32	b941
942	39	b942
943	-4	b943
944	3	b944
945	10	b945
946	17	b946
947	24	b947
948	31	b948
949	38	b949
950	-5	b950
951	2	b951
952	9	\N
953	16	b953
954	23	b954
955	30	b955
956	37	b956
957	44	b957
958	1	b958
959	8	b959
960	15	b960
961	22	b961
962	29	b962
963	36	b963
964	43	b964
965	0	b965
966	7	b966
967	14	b967
968	21	b968
969	28	\N
970	35	b970
971	42	b971
972	-1	b972
973	6	b973
974	13	b974
975	20	b975
976	27	b976
977	34	b977
978	41	b978
979	-2	b979
980	5	b980
981	12	b981
982	19	b982
983	26	b983
984	33	b984
985	40	b985
986	-3	\N
987	4	b987
988	11	b988
989	18	b989
990	25	b990
991	32	b991
992	39	b992
993	-4	b993
994	3	b994
995	10	b995
996	17	b996
997	24	b997
998	31	b998
999	38	b999
1000	-5	b1000
1001	2	b1001
1002	9	b1002
1003	16	\N
1004	23	b1004
1005	30	b1005
1006	37	b1006
1007	44	b1007
1008	1	b1008
1009	8	b1009
1010	15	b1010
1011	22	b1011
1012	29	b1012
1013	36	b1013
1014	43	b1014
1015	0	b1015
1016	7	b1016
1017	14	b1017
1018	21	b1018
1019	28	b1019
1020	35	\N
1021	42	b1021
1022	-1	b1022
1023	6	b1023
1024	13	b1024
1025	20	b1025
1026	27	b1026
1027	34	b1027
1028	41	b1028
1029	-2	b1029
1030	5	b1030
1031	12	b1031
\.

\s2 SELECT count(*), max(id) FROM r9c11858;

COMMIT;

\s2 SELECT count(*), max(id), sum(a) FROM r9c11858;

DROP TABLE r9c11858log;

DROP TABLE r9c11858;

DROP FUNCTION r9c11858fb(), r9c11858fa(), r9c11858fs();

PREPARE r9p36433 (int, int) AS SELECT n.id, n.a / $1 FROM r9n n JOIN r9s s ON s.id = n.id WHERE s.g = $2 ORDER BY n.id LIMIT 3;

EXECUTE r9p36433(1, 1);

-- statement
EXPLAIN (COSTS OFF) EXECUTE r9p36433(7, 8);
