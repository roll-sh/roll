-- class: error_detail
-- ordered: true
-- note: 42601
-- engine: ERROR 42601: syntax error at or near "="
-- engine: POSITION: 1073
-- reference: ERROR 42601: syntax error at or near "="
-- reference: POSITION: 1075

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int2, b json, c int4[]);

INSERT INTO t0 (id, a, b, c) VALUES (25, 3, NULL, '{1,1,2}'), (10, 3, 'null', '{NULL,3}'), (14, 0, NULL, NULL);

CREATE TABLE t1 (a text, b int4, c numeric(10,2), d d3int);

CREATE TABLE t1_c (extra int) INHERITS (t1);

INSERT INTO t1_c (a, b, c, d, extra) VALUES (NULL, 5, 3.05, NULL, 0);

INSERT INTO t1 (a, b, c, d) VALUES (NULL, NULL, NULL, 0), ('Ab', NULL, NULL, NULL), (NULL, NULL, NULL, NULL);

CREATE INDEX t1_i258 ON t1 (d) WHERE d IS NOT NULL;

CREATE TABLE t2 (id int4 PRIMARY KEY, a int4[], b text DEFAULT 'ba', c text COLLATE "C", d float8, e interval);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (29, '{5}', ' x', '10', 0.25, '1 mon'), (16, '{1,1,2}', 'Z', NULL, 2, NULL), (31, '{1,1,2}', '10', 'é', 2, '2 hours');

CREATE INDEX t2_i199 ON t2 (a);

CREATE INDEX t2_i636 ON t2 USING hash (a);

CREATE INDEX t2_i28 ON t2 (e DESC NULLS LAST);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int2 DEFAULT 6, b int4, c text COLLATE "en-x-icu");

INSERT INTO t3 (id, a, b, c) VALUES (29, 5, -1, 'Z');

CREATE INDEX t3_i62 ON t3 (b);

CREATE INDEX t3_i554 ON t3 (a);

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('a%', NULL)::ct3, NULL, 2), (2, ROW(NULL, 1)::ct3, ARRAY[ROW('ß', NULL)::ct3, ROW('10', -1)::ct3], NULL), (3, ROW('b', -1)::ct3, ARRAY[ROW('bar', -1)::ct3, ROW('a_b', 1)::ct3], 1), (4, ROW('B', -1)::ct3, '{}', 2);

CREATE INDEX tc3_c ON tc3 (c);

CREATE INDEX tc3_cd ON tc3 (c DESC NULLS LAST, n);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN false THEN width_bucket((2.50::numeric - p), 0, 10, 4) ELSE p END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := ((p % 1) % 1);
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(p, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT '2024-10-05 18:30:00'::timestamp AS c0 FROM t2 t1 WHERE '2024-08-21 08:00:00+00'::timestamptz = LEAST('2000-09-12 09:00:00-05'::timestamptz, ('2024-12-15 19:00:00-05'::timestamptz + t1.e));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '2025-03-09 02:30:00', '2025-03-09', '23:59:59.999999', '-2.5', '-1.25', 5), (2, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-03-09 01:30:00-05', '2000-01-01 00:00:00', '2025-03-09', '1 year 2 mons', '2.5', 'Infinity', NULL), (3, 'null', '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '1970-01-01 00:00:00.5', '2000-03-01', '23:59:59.999999', '1e30', NULL, 1), (4, '"str"', '"str"', '2025-10-26 01:30:00+00', '2025-10-26 02:30:00', '4713-01-01 BC', '-infinity', '1.23456789012345678901234567890', '2', 0), (5, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', NULL, 'infinity', '-infinity', '1.23456789012345678901234567890', '-1.25', NULL), (6, 'null', 'null', '2025-11-02 01:30:00-04', '1970-01-01 00:00:00.5', 'infinity', '0', '1.23456789012345678901234567890', '-0', -1), (7, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '2025-11-02 01:30:00', '2024-02-29', '1 day', '0.000001', '0.125', -1), (8, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', 'infinity', NULL, '2025-03-09', '36 hours', '0.5', '-1.25', 9), (9, NULL, NULL, NULL, '2025-11-02 01:30:00', 'infinity', 'P1DT12H', '0.5', '0.5', -1), (10, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-04-06 02:30:00+11', 'infinity', 'infinity', '23:59:59.999999', '-7', NULL, -3);

INSERT INTO tp4 VALUES (16, '-infinity', 'b', '1.23456789012345678901234567890'), (5, '1999-12-31', 'c', '-Infinity'), (0, '-infinity', 'a', NULL), (1, '1999-12-31', 'd', NULL), (13, '2025-03-09', 'ab', '99999999999999999999');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "pg_c_utf8", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 17, 'ä', '2.5', '-1 mon 30 days', '[0,10)', '{[1,3),[5,7)}', 'Fat cats! FAT rats? fat-free', '{}'), (2, 4, 'a-', '-Infinity', '1 day 00:00:00.000001', NULL, '{[0,100)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-2, 'a-')::r5ct]), (3, 5, 'ö', 'NaN', '2147483647 days', 'empty', '{[0,100)}', 'the and of', ARRAY[ROW(0, 'ss')::r5ct]), (4, NULL, 'é', 'NaN', '0', '(,2]', NULL, NULL, '{}'), (5, 18, 'å', NULL, '-1 mon 30 days', NULL, '{[2,4)}', 'The quick brown fox jumps over the lazy dog', NULL), (6, 4, 'b', '0.0000000000000000000000000001', NULL, '[-3,0]', '{[0,100)}', 'running runners ran; the runner runs', ARRAY[ROW(2, NULL)::r5ct, ROW(1, 'SS')::r5ct, ROW(-2, 'ae')::r5ct]), (7, 20, NULL, '7', '30 days', '[3,)', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(1, NULL)::r5ct, ROW(3, 'ss')::r5ct]), (8, 18, 'Ab', '-1e40', '2147483647 mons', 'empty', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(4, NULL)::r5ct, ROW(-1, 'b')::r5ct, ROW(-2, 'İ')::r5ct]), (9, 8, 'a', NULL, '-178000000 years', NULL, '{[1,3),[5,7)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(5, 'ch')::r5ct, ROW(3, 'Ab')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -1 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 26 WITH LOCAL CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (-1, 'a', '2025-01-01'), (10, 'b', 'infinity'), (3, 'c', NULL), (16, 'd', NULL), (7, 'a', NULL), (6, 'b', NULL), (17, 'c', '2024-02-29'), (12, 'd', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (31, 'x0', NULL), (30, 'x1', NULL), (25, 'x2', NULL), (28, 'x3', NULL), (25, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (2, 'u2@x.org', 'bob2', 2, '{b,NULL}', '[1, {"a": null}]'), (3, 'u3@x.org', 'bob3', 3, '{a}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, 100, 'paid', '2025-02-07', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 9.99, 'void', '2025-02-04', NULL), (3, 9.99, 'void', '2025-02-17', NULL);

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

INSERT INTO r7k VALUES (1, 1, 121, 'Note 3', -12.125, 0.5, '2024-02-29 12:00:00+00'), (2, 1, -37, '', NULL, NULL, '2025-03-09 07:00:00+00'), (3, 1, 148, 'NOTE 3', 1000, 2.25, '2025-11-02 05:30:00+00'), (4, 1, 86, 'é', -12.125, NULL, '2025-03-09 07:00:00+00'), (5, 2, 130, '', -12.125, -0, '2024-02-29 12:00:00+00'), (6, 1, 114, 'note 3', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (7, 1, 107, 'note 1', 0, -0, '2025-03-09 06:59:59+00'), (8, 1, 178, 'note 3', 3.333, NULL, '2025-03-09 06:59:59+00'), (9, 1, -24, 'n%5', 1000, NULL, '2024-02-29 12:00:00+00'), (10, 7, 61, 'note_4', 0.5, -0, '2025-03-09 07:00:00+00'), (11, 1, 109, 'note_4', NULL, 'Infinity', '2025-11-02 05:30:00+00'), (12, 5, -10, 'n%5', 0.5, -0, '2025-03-09 06:59:59+00'), (13, 1, 12, 'note 3', 0.5, 1e300, '2025-11-02 05:30:00+00'), (14, 1, NULL, 'NOTE 3', NULL, NULL, '2025-03-09 06:59:59+00'), (15, 1, 48, 'note 2', 3.333, 0.5, '2025-03-09 07:00:00+00'), (16, 1, 52, 'Note 3', NULL, 1e300, '2025-11-02 05:30:00+00'), (17, 1, 70, 'note 1', 0.5, NULL, '2025-03-09 07:00:00+00'), (18, 1, 120, 'é', 3.333, NULL, '2024-02-29 12:00:00+00'), (19, 1, 140, 'note 2', 0, 'Infinity', '2025-03-09 07:00:00+00'), (20, 3, 184, 'n%5', -12.125, 'Infinity', '2025-11-02 05:30:00+00'), (21, 9, 144, 'n%5', 0, NULL, '2025-03-09 06:59:59+00'), (22, 1, 193, 'note 2', 1000, NULL, '2024-02-29 12:00:00+00'), (23, 1, 114, '', 0, 'NaN', '2025-03-09 06:59:59+00'), (24, 6, NULL, 'NOTE 3', 0.5, 2.25, '2025-11-02 05:30:00+00'), (25, 1, -13, 'é', 0, 'NaN', '2025-03-09 06:59:59+00'), (26, 1, 52, 'note 2', -12.125, 'NaN', '2025-03-09 07:00:00+00'), (27, 4, 1, 'note 2', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (28, 1, 3, 'note_4', 1000, NULL, '2024-02-29 12:00:00+00'), (29, 5, 97, 'é', 1000, NULL, '2025-03-09 06:59:59+00'), (30, 1, NULL, '', 1000, 'NaN', '2024-02-29 12:00:00+00'), (31, 3, -48, 'note 1', -12.125, -0, NULL), (32, 1, 53, 'note_4', 0, 'NaN', '2024-02-29 12:00:00+00'), (33, 1, 19, 'note 3', NULL, 1e300, '2025-11-02 05:30:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21);

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

INSERT INTO r7t VALUES (1, 'Ä', 'Ä'), (2, 'b', 'b'), (3, 'é', 'é'), (4, 'a', 'a'), (5, 'ss', 'ss'), (6, '10', '10');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, 5, 10, 'z', '2025-01-01'::timestamp + interval '52 hours'), (2, 1, 11, -2.25, 'b', '2025-01-01'::timestamp + interval '13 hours'), (3, 2, 11, 1.50, 'B', '2025-01-01'::timestamp + interval '192 hours'), (4, 3, 3, -2.25, NULL, '2025-01-01'::timestamp + interval '56 hours'), (5, 1, -3, 0, NULL, '2025-01-01'::timestamp + interval '56 hours'), (6, 3, 13, 3.75, 'B', '2025-01-01'::timestamp + interval '150 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 2), (2, 1, 3), (2, 5, 7), (3, 1, 9), (3, 6, 8), (4, 4, 5), (5, 3, 1), (6, 4, 4), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '(-3,2]', '{}', '[2025-01-01,2025-02-01)', '(1.25,1.5)'), (2, NULL, '{[1,1), [3, 5)}', '[2025-03-01,infinity)', '(,0]'), (3, '[8,)', '{(,8)}', NULL, '(1.25,1.5)'), (4, '[5,)', '{(,5)}', '[2025-03-01,infinity)', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, ''), (2, ''), (3, 'running rats'), (4, 'dog-eat-dog world'), (5, 'jumps over the lazy dog'), (6, 'a rat ran'), (7, 'a rat ran'), (8, 'brown''s fox''s den');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (4, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3703) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1960) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3400) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r9a19518 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a19518 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4378, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4378) i;

ALTER TABLE r9a19518 ALTER COLUMN f SET STATISTICS -1;

SET default_statistics_target = 15;

CREATE STATISTICS r9a19518_st (ndistinct, dependencies) ON g, (u % 10) FROM r9a19518;

UPDATE r9a19518 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a19518 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a19518 WHERE s LIKE 'v1%'$$);

DROP TABLE r9a19518;

CREATE TABLE r8ex48646 (id int, room int, during tsrange, EXCLUDE USING gist (during WITH &&));

INSERT INTO r8ex48646 VALUES (1, 1, '[2025-01-01 10:00, 2025-01-01 11:00)');

INSERT INTO r8ex48646 VALUES (2, 1, '[2025-01-01 11:00, 2025-01-01 12:00)');

INSERT INTO r8ex48646 VALUES (3, 2, 'empty'), (4, 2, 'empty');

DROP TABLE r8ex48646;

CREATE VIEW r8v85975 AS SELECT id, v, upper(t) AS ut FROM r8a WHERE id > 100;

CREATE FUNCTION r8vf85975() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN INSERT INTO r8a (id, v, t) VALUES (NEW.id, NEW.v, lower(NEW.ut)); RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN UPDATE r8a SET v = NEW.v WHERE id = OLD.id; RETURN NEW;
  ELSE DELETE FROM r8a WHERE id = OLD.id; RETURN OLD; END IF;
END $$;

CREATE TRIGGER r8vt INSTEAD OF INSERT OR UPDATE OR DELETE ON r8v85975 FOR EACH ROW EXECUTE FUNCTION r8vf85975();

INSERT INTO r8v85975 VALUES (1076, 5, 'Q') RETURNING *;

UPDATE r8v85975 SET v = v + 1 WHERE id = 1076 RETURNING *;

DELETE FROM r8v85975 WHERE id = 1076 RETURNING id, ut;

DROP VIEW r8v85975;

DROP FUNCTION r8vf85975;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = auto; FOR i IN 0..11 LOOP s := s + coalesce((SELECT count(*) FROM r9n WHERE a > i * 1000) / (i % 3), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

\s2 SET plan_cache_mode = force_generic_plan;

\x2 name=r9x92400 types=23 describe p=b00000005
SELECT count(*), sum(v), min(t) FROM r9s WHERE g = $1;

\x2 name=r9x92400 rf=1 p=t'1'
-;

\x2 name=r9x92400 p=t'1'
-;

\x2 name=r9x92400 p=t'5'
-;

\x2 name=r9x92400 p=t'3'
-;

\x2 name=r9x92400 p=b00000003
-;

\x2 name=r9x92400 rf=0 p=b00000003
-;

\x2 name=r9x92400 rf=1,0 p=b00000003
-;

\x2 name=r9x92400 p=t'8'
-;

\x2 name=r9x92400 rf=0 p=b00000001
-;

\x2 name=r9x92400 rf=0 p=t'99'
-;

\x2 name=r9x92400 rf=1,0 p=t'1'
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x92400';

\s2 DEALLOCATE r9x92400;

\s2 RESET plan_cache_mode;

SET enable_bitmapscan = off;

RESET enable_bitmapscan;

SET enable_indexonlyscan = off;

EXPLAIN (COSTS OFF) SELECT max(id), min(id), max(v) FROM r9s WHERE id < 481;

RESET enable_indexonlyscan;

SET work_mem = '64kB';

SET enable_hashjoin = off;

EXPLAIN (COSTS OFF) SELECT count(DISTINCT s), count(DISTINCT k), string_agg(DISTINCT left(s, 1), '' ORDER BY left(s, 1)) FROM r8big;

RESET work_mem;

RESET enable_hashagg;

RESET enable_sort;

RESET enable_mergejoin;

RESET hash_mem_multiplier;

RESET enable_hashjoin;

CREATE TABLE r9a46731 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a46731 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1439, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1439) i;

ALTER TABLE r9a46731 ALTER COLUMN s SET STATISTICS 100;

UPDATE r9a46731 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a46731;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a46731 WHERE s = 'v7'$$);

SELECT * FROM r9est($$SELECT * FROM r9a46731 WHERE s = 'common'$$);

DROP TABLE r9a46731;

CREATE TABLE r9i11270 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i11270 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 187) i;

ANALYZE r9i11270;

CREATE INDEX r9i11270_x0 ON r9i11270 USING btree (t COLLATE "C");

INSERT INTO r9i11270 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(188, 333) i;

UPDATE r9i11270 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i11270 WHERE id % 11 = 0;

SET enable_seqscan = off;

SET enable_bitmapscan = off;

EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i11270 ORDER BY p <-> point '(3,3)', id LIMIT 7;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i11270;

SELECT 'empty'::int4range < int4range(3, 7), 'empty'::int4range + int4range(3, 7), range_merge('empty'::int4range, int4range(3, 7)), lower('empty'::int4range), upper_inc(int4range(3, 7)), isempty('empty'::int4range * int4range(3, 7)), lower_inf('empty'::int4range);

CREATE FUNCTION r8f71965(a int[]) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  FOREACH x SLICE 0 IN ARRAY a LOOP RETURN NEXT x::text; END LOOP;
  RETURN;
END $$;

SELECT * FROM r8f71965('{{1,2},{3,4}}');

SELECT * FROM r8f71965('{5,NULL,7}');

SELECT * FROM r8f71965('{}');

SELECT * FROM r8f71965(NULL);

DROP FUNCTION r8f71965;

CREATE TABLE r9a9732 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a9732 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4641, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4641) i;

ALTER TABLE r9a9732 ALTER COLUMN u SET (n_distinct = 50);

ANALYZE r9a9732;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a9732 WHERE g = 2 AND s = 'common'$$);

DROP TABLE r9a9732;

CREATE TABLE r9a78837 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a78837 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 1988, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 1988) i;

ALTER TABLE r9a78837 ALTER COLUMN g SET STATISTICS 0;

CREATE STATISTICS r9a78837_st ON g, (u % 10) FROM r9a78837;

ANALYZE r9a78837;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a78837 WHERE nul > 100$$);

EXPLAIN SELECT * FROM r9a78837 WHERE nul > 100;

SELECT * FROM r9est($$SELECT * FROM r9a78837 WHERE s LIKE 'v1%'$$);

DROP TABLE r9a78837;

CREATE TABLE r8c57691 (id int NOT NULL, a text, b int DEFAULT 42, c date);

COPY r8c57691 (id, b, a, c) FROM STDIN (FORMAT csv, HEADER MATCH);
id,a,x,c
1,\D,1,2025-13-01
2,x,99999999999,,extra
,x,1,
\.

COPY r8c57691 TO STDOUT (FORMAT binary, HEADER);

COPY r8c57691 FROM STDIN (FORMAT csv, FORCE_QUOTE (a));

DROP TABLE r8c57691;

-- statement
(SELECT string_agg(LEAST('bar', 'a'), ',' ORDER BY LEAST('bar', 'a')) AS c0, ((SELECT (s4.c0 / 7) FROM t2 t8 WHERE s1.c0 ORDER BY 1 LIMIT 1) * 1e20::numeric) AS c1 FROM ((SELECT NULL::bool AS c0, '{"a": null}'::jsonb AS c1 FROM (v0 t2 JOIN v0 t3 ON t2.c0 = t3.c0) WHERE COALESCE(NULL::text !~ 'A%', false)) s1 RIGHT JOIN LATERAL (SELECT (round(('NaN'::numeric - t5.b), 1))::numeric(6,1) AS c0, (cardinality(('{1,2}'::int4[] || t5.d)))::int2 AS c1, t5.c AS c2, ((CASE WHEN s1.c0 THEN '2000-01-28 15:00:00+02:30'::timestamptz ELSE '2000-07-19 13:00:00+00'::timestamptz END + interval '24 hours'))::text::timestamptz AS c3 FROM (t1 t5 LEFT JOIN t2 t6 ON t5.a = t6.c) WHERE false) s4 ON NOT EXISTS (SELECT DISTINCT f0(t7.b, regexp_replace(('x ' COLLATE "POSIX"), '[ab]', 'y', 'g')) FROM t3 t7 WHERE 'Ä' IS DISTINCT FROM t7.c)) GROUP BY GROUPING SETS ((s1.c1), (s4.c0)) HAVING s1.c1 IS NOT NULL) UNION (SELECT concat('ß', 10) AS c0, (ceil(1e3::numeric) - jsonb_array_length('{}'::jsonb)) AS c1 FROM (t1 t9 JOIN t2 t10 ON t9.b = t10.id AND '[]'::jsonb ?| ARRAY['a', 'x'] <> t10.a = t10.a) WHERE (t9.d, upper(array_to_string('{b,NULL}'::text[], ',', '?'))) > (t10.id, (split_part(t10.b, ',', 1) || upper(t10.b)))) ORDER BY 1 DESC, 2 DESC NULLS LAST;
