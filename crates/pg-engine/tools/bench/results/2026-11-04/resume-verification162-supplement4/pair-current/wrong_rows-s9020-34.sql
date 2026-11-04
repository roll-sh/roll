-- class: wrong_rows
-- ordered: false
-- note: 3 vs 3 rows
-- engine: QUERY PLAN:25
-- engine: HashAggregate  (cost=94.87..114.87 rows=1600 width=8)
-- engine:   Group Key: g, (u % 10)
-- engine:   ->  Seq Scan on r9a95340  (cost=0.00..80.05 rows=2964 width=8)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: HashAggregate  (cost=94.87..131.92 rows=2964 width=8)
-- reference:   Group Key: g, (u % 10)
-- reference:   ->  Seq Scan on r9a95340  (cost=0.00..80.05 rows=2964 width=8)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4, b interval, c timestamptz);

INSERT INTO t0 (id, a, b, c) VALUES (15, NULL, NULL, NULL), (24, 7, NULL, NULL), (10, 9, NULL, NULL), (25, 3, '1 day', NULL), (34, 8, NULL, NULL), (3, NULL, NULL, NULL), (16, 7, '30 days', '2024-01-03 06:00:00+00'), (36, NULL, NULL, NULL);

CREATE INDEX t0_i86 ON t0 USING hash (c);

CREATE INDEX t0_i911 ON t0 (b);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 THEN RETURN NULL; END IF; NEW.a := coalesce(NEW.a, -1); RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER DELETE ON t0 REFERENCING OLD TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a int4, b text COLLATE "ci3", c date, d text, e int4, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (16, 8, 'A', NULL, 'bar', 3), (29, NULL, NULL, '2024-02-21', 'A', 1), (32, 0, NULL, '2024-03-28', 'Ab', NULL), (36, NULL, NULL, '2024-03-17', 'ba', NULL), (59, NULL, NULL, '2024-02-15', 'Ab', 5), (20, 8, 'x ', NULL, NULL, NULL), (28, NULL, NULL, NULL, NULL, 7), (30, NULL, 'aa', '2024-02-05', 'ß', 0), (18, 5, NULL, '2024-01-24', 'a%', NULL), (24, 5, NULL, NULL, 'ab', 1), (13, 7, 'Ä', '2024-01-14', 'aa', 1), (6, 5, 'b', NULL, NULL, 5), (11, 7, NULL, '2024-03-03', '9', 7), (12, 7, '10', NULL, 'foo', NULL), (40, 1, 'z', '2024-02-05', 'a,b', -2), (5, NULL, 'Z', NULL, NULL, NULL), (33, NULL, 'abc', '2024-03-01', 'Ab', 9), (3, NULL, NULL, '2024-01-02', 'a%', NULL), (39, 5, 'z', '2024-02-23', NULL, 7), (17, 7, 'B', '2024-01-14', 'ß', 4);

CREATE INDEX t1_i363 ON t1 (b);

CREATE INDEX t1_i56 ON t1 (g);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE STRICT AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN q < q COLLATE "POSIX" THEN RETURN p;
  ELSE RETURN (COALESCE('1'::jsonb, '{"a": null}'::jsonb)::jsonb ->> 'a')::int;
  END CASE;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
DECLARE r int := 0;
BEGIN
  IF NULLIF('2000-10-27 19:00:00-05'::timestamptz > NULL::timestamptz, 7 NOT IN (SELECT abs(GREATEST(p, (p + p))) AS c0 FROM (SELECT 12345678.9::numeric AS c0, ('2024-09-19 04:00:00'::timestamp - t2.b) AS c1, ('2025-08-23 13:30:00'::timestamp - '2024-08-05 05:00:00'::timestamp) AS c2 FROM (t0 t2 FULL JOIN t1 t3 ON ('"s"'::json -> 'a') IS NOT NULL) WHERE ('[]'::jsonb ->> 'a') NOT BETWEEN trim(left(t3.d, 0)) AND initcap(t3.d)) s1)) THEN
    r := ((SELECT array_agg(3 ORDER BY 3 NULLS FIRST) FILTER (WHERE q <= q) FROM ONLY t0 t4 WHERE true))[1];
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(ascii((q || q)), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT '2000-01-02 16:00:00+00'::timestamptz AS c0, (- (- 2::float8)) AS c1, ('1999-11-07 08:30:00'::timestamp - CASE WHEN true THEN '2025-12-17 19:30:00'::timestamp ELSE '1999-12-16 04:30:00'::timestamp END) AS c2, jsonb_agg(CASE WHEN false THEN jsonb_array_length('"s"'::jsonb) ELSE extract(day from '2024-11-22'::date)::int END ORDER BY CASE WHEN false THEN jsonb_array_length('"s"'::jsonb) ELSE extract(day from '2024-11-22'::date)::int END) AS c3 FROM generate_series(1, 2) s1(g) WHERE false GROUP BY s1.g HAVING bool_and('foo' NOT LIKE '^a');

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-04-06 02:30:00+11', '2000-01-01 00:00:00', '1999-12-31', '-1 mon 2 days', '-1e-20', '-3', -2), (2, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', 'infinity', NULL, '2000-03-01', '-12:00', '-0.0', '2', 1), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-10-26 01:30:00+00', '1970-01-01 00:00:00.5', '4713-01-01 BC', '1 year 2 mons', '-7', '-0', 3), (4, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-03-09 01:30:00-05', '0001-01-01 00:00:00 BC', 'infinity', '-1 year -1 day', '1.23456789012345678901234567890', '-3', 11), (5, '"str"', '"str"', '2025-10-26 01:30:00+00', '1970-01-01 00:00:00.5', '2025-01-01', '-1 mon 2 days', '1e30', '-0', -3), (6, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2024-02-29 23:00:00+00', 'infinity', '1999-12-31', 'P1DT12H', '-1e-20', 'NaN', 11), (7, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '1999-12-31 23:59:59.999999+00', '2024-02-29 00:00:00', '1999-12-31', NULL, '1e30', '2', 0), (8, NULL, NULL, '2025-03-09 01:30:00-05', NULL, '4713-01-01 BC', 'P1DT12H', 'Infinity', '-0', NULL), (9, 'null', 'null', '2025-11-02 01:30:00-05', NULL, '2025-03-09', 'infinity', '0.5', '-3', 10), (10, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '-infinity', '2025-12-31 23:59:59.999999', 'infinity', '0', 'NaN', 'Infinity', -1), (11, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', NULL, NULL, '-infinity', '1 day', '-7', '-0', 7), (12, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '2000-01-01 00:00:00', '2000-03-01', '2 weeks 3 microseconds', '1.23456789012345678901234567890', '1024', NULL), (13, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-11-02 01:30:00-05', 'infinity', NULL, NULL, '-2.5', '-1.25', -2), (14, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', NULL, NULL, '-1 mon 2 days', '123.45', 'NaN', 0);

INSERT INTO tp4 VALUES (3, NULL, 'b', NULL), (23, NULL, 'ab', '123.45'), (8, '2025-03-09', 'ab', '123.45'), (23, '2024-02-29', 'b', '0'), (4, '4713-01-01 BC', NULL, '-1e-20'), (4, 'infinity', 'd', NULL), (5, '-infinity', 'd', NULL), (23, '1999-12-31', 'd', '-1e-20'), (5, '-infinity', NULL, 'NaN'), (12, '2025-03-09', 'a', '0');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "en-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 20, ' a', 'NaN', '-1 mon 30 days', '(,)', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(-2, 'ch')::r5ct, ROW(3, 'ch')::r5ct]), (2, 20, 'å', '-1e40', '1 year -1 sec', '[5,6)', '{[2,4)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(NULL, ' a')::r5ct, ROW(0, 'ı')::r5ct, ROW(0, '2')::r5ct]), (3, 9, '9', '1e-130', '-1 mon 30 days', '[-3,0]', '{(,0],[10,)}', 'the and of', ARRAY[ROW(0, '-a')::r5ct, ROW(0, 'ß')::r5ct]), (4, -2, 'a-', '-0.5', '2147483647 days', '[0,10)', NULL, '', '{}'), (5, 3, 'a', '-1e40', '720:00:00', 'empty', '{[2,4)}', NULL, ARRAY[ROW(-2, 'é')::r5ct, ROW(-2, 'Ab')::r5ct, ROW(2, 'a-')::r5ct]), (6, 2, 'å', '2.5', '720:00:00', '[-3,0]', '{[1,3),[5,7)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(-2, 'Ä')::r5ct, ROW(1, NULL)::r5ct, ROW(3, 'h')::r5ct]), (7, 13, NULL, NULL, '-1 mon 30 days', '(,)', '{[0,100)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(1, 'ss')::r5ct, ROW(5, NULL)::r5ct]), (8, 10, '2', '0', '1 year -1 sec', '(,2]', '{(,0],[10,)}', 'the and of', ARRAY[ROW(-2, 'o')::r5ct]);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 20 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (2, 'a', NULL), (3, 'b', 'infinity'), (0, 'c', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL), (26, 'x1', NULL), (25, 'x2', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Cid1', 0, '{a}', '[1, {"a": null}]'), (2, 'u2@x.org', 'ann2', 2, '{a,b}', '[1, {"a": null}]'), (3, 'u3@x.org', 'ann3', 3, '{}', NULL), (4, 'u4@x.org', 'bob 4', 0, NULL, '{}'), (5, 'u5@x.org', 'dee5', 5, '{a}', '{}'), (6, 'u6@x.org', 'bob6', 6, '{b,NULL}', '[1, {"a": null}]'), (7, 'u7@x.org', 'Cid7', 0, '{a,b}', '{"seats": 1.50}'), (8, 'u8@x.org', 'Cid8', 8, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (9, 'u9@x.org', 'dee9', 9, '{a}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (5, NULL, 'void', '2025-02-11', NULL), (5, 12345678.90, 'new', '2025-02-16', NULL), (9, 12345678.90, 'new', '2025-02-10', NULL), (6, NULL, 'void', '2025-02-03', '{"items": []}'), (2, NULL, 'new', '2025-02-16', '{"gift": true}'), (6, 100, 'new', '2025-02-28', '{"items": []}'), (6, 100, 'void', '2025-02-12', NULL), (9, 9.99, 'paid', '2025-02-16', NULL), (3, -1.50, 'void', '2025-02-22', NULL), (4, -1.50, 'paid', '2025-02-17', '{"items": []}'), (6, 9.99, 'void', '2025-02-16', '{"gift": true}'), (3, 100, 'paid', '2025-02-20', '{"items": [{"sku": "a", "qty": 2}]}'), (7, NULL, 'new', '2025-02-18', '{"gift": true}'), (5, 100, 'paid', '2025-02-07', NULL), (2, 12345678.90, 'void', '2025-02-06', '{"items": []}'), (1, NULL, 'new', '2025-02-11', NULL), (6, 9.99, 'paid', '2025-02-28', NULL), (7, 12345678.90, 'paid', '2025-02-12', '{"gift": true}'), (6, -1.50, 'void', '2025-02-26', '{"gift": true}'), (8, 0, 'void', '2025-02-20', '{"gift": true}'), (9, 9.99, 'new', '2025-02-09', '{"gift": true}'), (1, 12345678.90, 'paid', '2025-02-13', '{"gift": true}'), (4, -1.50, 'void', '2025-02-26', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, 49, 'NOTE 3', NULL, -0, NULL), (2, 1, -36, 'é', 0, NULL, '2025-11-02 05:30:00+00'), (3, 1, -31, 'note 3', 0.5, 1e300, '2025-11-02 05:30:00+00'), (4, 1, -11, 'note 3', -12.125, 'NaN', '2025-03-09 06:59:59+00'), (5, 1, 176, 'note 1', -12.125, 1e300, '2025-03-09 07:00:00+00'), (6, 4, -28, 'note_4', NULL, 0.5, '2024-02-29 12:00:00+00'), (7, 1, 149, 'note 3', 1000, -0, '2024-02-29 12:00:00+00'), (8, 6, 189, 'n%5', -12.125, 0.5, '2025-03-09 06:59:59+00'), (9, 1, -37, 'Note 3', 1000, -0, '2025-03-09 07:00:00+00'), (10, 6, 165, 'n%5', 0, 0.5, NULL), (11, 1, 13, 'NOTE 3', 0.5, 'NaN', NULL), (12, 1, 99, '', 0.5, NULL, '2025-11-02 05:30:00+00'), (13, 1, 79, '', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (14, 1, 154, 'NOTE 3', NULL, 0.5, '2025-03-09 07:00:00+00'), (15, 4, 123, 'Note 3', 0, 1e300, '2025-03-09 07:00:00+00'), (16, 1, -50, '', 0.5, 1e300, NULL), (17, 1, -21, 'Note 3', NULL, 'Infinity', '2025-03-09 07:00:00+00'), (18, 2, -44, 'note 1', 0, 'NaN', '2024-02-29 12:00:00+00'), (19, 1, 142, 'note 1', -12.125, 2.25, '2025-03-09 07:00:00+00'), (20, 1, NULL, 'note 1', NULL, 'Infinity', '2025-03-09 06:59:59+00'), (21, 1, 56, 'note_4', 0.5, 2.25, '2025-03-09 06:59:59+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10), (9, 'us', 17);

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

INSERT INTO r7t VALUES (1, 'a', 'a'), (2, '10', '10'), (3, 'b', 'b'), (4, '10', '10'), (5, 'e', 'e'), (6, 'a', 'a'), (7, 'e', 'e'), (8, 'e', 'e'), (9, 'ä', 'ä'), (10, 'a', 'a');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, 1, 3.75, 'b', '2025-01-01'::timestamp + interval '30 hours'), (2, 3, 1, 1.50, NULL, '2025-01-01'::timestamp + interval '100 hours'), (3, 2, 20, 10, 'z', '2025-01-01'::timestamp + interval '3 hours'), (4, 1, 15, NULL, 'a', '2025-01-01'::timestamp + interval '37 hours'), (5, 1, -1, NULL, 'z', '2025-01-01'::timestamp + interval '134 hours'), (6, 4, 16, 10, 'a', '2025-01-01'::timestamp + interval '150 hours'), (7, 2, NULL, 10, 'a', '2025-01-01'::timestamp + interval '47 hours'), (8, 4, 10, 0, 'b', '2025-01-01'::timestamp + interval '124 hours'), (9, 1, NULL, NULL, 'ab', '2025-01-01'::timestamp + interval '108 hours'), (10, 3, -3, -2.25, NULL, '2025-01-01'::timestamp + interval '18 hours'), (11, 2, 15, 10, 'a', '2025-01-01'::timestamp + interval '54 hours'), (12, 2, 11, 3.75, 'z', '2025-01-01'::timestamp + interval '6 hours'), (13, 1, NULL, 3.75, NULL, '2025-01-01'::timestamp + interval '207 hours'), (14, 4, 7, 3.75, 'a', '2025-01-01'::timestamp + interval '38 hours'), (15, 3, 9, 0, 'b', '2025-01-01'::timestamp + interval '56 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 4, 5), (1, 7, 5), (3, 1, 3), (3, 5, 8), (4, 3, 4), (4, 3, 7), (7, 4, 7), (7, 4, 4), (8, 5, 7), (8, 4, 1), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-2,)', '{(,-2)}', '[2025-01-01,2025-02-01)', NULL), (2, '(-1,4]', '{[-1,4), [6, 8)}', '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (3, '[5,7)', '{[5,7), [9, 11)}', NULL, '[1.5,2.5)'), (4, '(4,4]', '{}', '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (5, '(4,6]', '{}', '[2025-01-01,2025-02-01)', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'foxes and dogs'), (2, 'über café naïve'), (3, ''), (4, 'the quick brown fox'), (5, ''), (6, 'brown''s fox''s den'), (7, 'jumps over the lazy dog'), (8, 'quick quick slow'), (9, 'running rats');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '"scalar"'), (6, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2727) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2326) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3590) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r8t45133 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t45133 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf45133() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t45133', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf45133_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t45133', r);
  RETURN NULL;
END $$;

CREATE CONSTRAINT TRIGGER h_def AFTER INSERT OR UPDATE ON r8t45133 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION r8tf45133('deferred');

CREATE TRIGGER g_ti AFTER INSERT ON r8t45133 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r8tf45133_tt();

CREATE CONSTRAINT TRIGGER h_def AFTER INSERT OR UPDATE ON r8t45133 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION r8tf45133('deferred');

CREATE TRIGGER b_air AFTER INSERT OR DELETE ON r8t45133 FOR EACH ROW EXECUTE FUNCTION r8tf45133();

INSERT INTO r8t45133 VALUES (10, 2, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't45133';

UPDATE r8t45133 SET v = v + 1 WHERE id = 1 RETURNING id, v, s;

DELETE FROM r8log WHERE tg = 't45133';

INSERT INTO r8t45133 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t45133.v RETURNING *;

DELETE FROM r8log WHERE tg = 't45133';

UPDATE r8t45133 SET v = coalesce(v, 0) - 1 RETURNING id, v;

DELETE FROM r8log WHERE tg = 't45133';

INSERT INTO r8t45133 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't45133';

DROP TABLE r8t45133;

DROP FUNCTION r8tf45133, r8tf45133_tt;

CREATE TABLE r9a95340 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a95340 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2964, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2964) i;

VACUUM ANALYZE r9a95340;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a95340$$);

-- statement
EXPLAIN SELECT DISTINCT g, u % 10 FROM r9a95340;
