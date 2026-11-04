-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a11837  (rows=1)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a11837  (rows=67)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a interval, b numeric(10,2) DEFAULT 7.75, c text[], g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b, c) VALUES (29, NULL, NULL, '{}');

CREATE INDEX t0_i449 ON t0 (a) WHERE a IS NOT NULL;

CREATE INDEX t0_i547 ON t0 (g);

CREATE INDEX t0_i17 ON t0 USING hash (c);

CREATE TABLE t1 (a int2, b int2, c text COLLATE "und-x-icu", d json);

INSERT INTO t1 (a, b, c, d) VALUES (NULL, NULL, 'abc', 'null'), (NULL, 5, 'A', '"s"'), (5, 5, 'abc', '{"a": "1"}'), (5, 5, 'é', '[]'), (NULL, 5, 'bar', '{"b": [1, "a"]}'), (NULL, 2, 'A', '{"a": "1"}'), (1, 5, 'Ab', '{"a": {"b": 2}}'), (2, NULL, 'a_b', '1'), (5, 6, 'Z', 'null'), (1, 0, 'ß', '"s"'), (4, NULL, 'b', NULL), (5, 0, 'ß', '{"a": "1"}');

CREATE INDEX t1_i382 ON t1 (a);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql  STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := p;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(4, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := abs((NULL::jsonb::jsonb ->> 'a')::int);
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce((abs(10.00::numeric))::int4, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT ('2025-12-23 09:59:00'::timestamp AT TIME ZONE 'UTC') AS c0 FROM t0 t1 WHERE t1.b IN (SELECT t3.b FROM (t1 t2 LEFT JOIN t0 t3 ON (false) IS NOT TRUE));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-10-26 01:30:00+00', '1970-01-01 00:00:00.5', NULL, '1 day', '0', 'NaN', 9), (2, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2038-01-19 03:14:08+00', '0001-01-01 00:00:00 BC', '1999-12-31', 'infinity', '1e30', NULL, -2), (3, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', 'infinity', '0001-01-01 00:00:00 BC', NULL, '-1 mon 2 days', '-7', 'NaN', 5), (4, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-03-09 03:30:00-04', NULL, '4713-01-01 BC', 'P1DT12H', NULL, NULL, NULL), (5, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-10-26 00:30:00+00', '2000-01-01 00:00:00', '2025-03-09', '-12:00', '1.23456789012345678901234567890', NULL, 3), (6, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', NULL, NULL, '-12:00', '-7', '-1.25', NULL), (7, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '2024-02-29 23:00:00+00', '2025-12-31 23:59:59.999999', '2000-03-01', '-1 year -1 day', '-7', 'NaN', 2), (8, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', 'infinity', '2025-12-31 23:59:59.999999', '2025-03-09', '23:59:59.999999', '-Infinity', '-0', 4), (9, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-04-06 02:30:00+11', '2024-02-29 00:00:00', '2025-03-09', '-infinity', NULL, 'Infinity', 7), (10, NULL, NULL, '-infinity', '2025-10-26 02:30:00', '2025-01-01', '-infinity', '-2.5', '-0', 0), (11, NULL, NULL, NULL, '2025-03-09 02:30:00', '2025-01-01', 'P1DT12H', 'Infinity', 'Infinity', 4), (12, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', '2025-12-31 23:59:59.999999', '2000-03-01', 'infinity', 'NaN', '0.125', 0), (13, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', 'infinity', '0001-01-01 00:00:00 BC', NULL, '-1 year -1 day', '1e30', NULL, NULL);

INSERT INTO tp4 VALUES (17, '4713-01-01 BC', 'a', '99999999999999999999'), (23, 'infinity', 'b', '-7'), (23, '4713-01-01 BC', 'd', '-0.0'), (15, '4713-01-01 BC', 'd', 'Infinity'), (18, '1999-12-31', NULL, '-0.0'), (-3, '2025-01-01', 'c', '-7'), (9, '2025-03-09', 'd', '-2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "ucs_basic", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 10, 'a-', NULL, NULL, '[0,10)', '{(,0],[10,)}', '', NULL), (2, 3, 'ö', '1e-130', '2147483647 mons', '[3,)', '{[0,100)}', 'a fat cat sat on a mat and ate a fat rat', NULL), (3, 7, 'Z', '7', '1 year -1 sec', '[1,5)', '{[2,4)}', 'running runners ran; the runner runs', NULL), (4, 8, 'ß', '2.5', 'infinity', NULL, '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', '{}'), (5, 8, '10', 'Infinity', '-infinity', '[0,10)', '{[1,3),[5,7)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL), (6, 14, 'a-', '-99999999999999999999999999999999999.999999', '30 days', 'empty', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL), (7, -3, 'Ä', NULL, '1 mon', NULL, NULL, 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(-1, ' a')::r5ct]), (8, 9, 'i', '0', '30 days', '[5,6)', NULL, 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-2, 'i')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 8;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (14, 'a', 'infinity'), (18, 'b', NULL), (8, 'c', '2024-02-29'), (10, 'd', NULL), (13, 'a', '2024-02-29'), (-3, 'b', '2025-01-01'), (16, 'c', 'infinity'), (17, 'd', '2024-02-29'), (6, 'a', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (20, 'x0', NULL), (30, 'x1', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Cid1', 1, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (2, 'u2@x.org', '2', 2, NULL, '[1, {"a": null}]'), (3, 'u3@x.org', 'bob 3', 3, '{}', '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 100, 'new', '2025-02-09', '{"gift": true}'), (1, 9.99, 'paid', '2025-02-28', '{"items": []}'), (1, -1.50, 'new', '2025-02-15', NULL), (3, -1.50, 'paid', '2025-02-28', '{"gift": true}'), (3, 100, 'void', '2025-02-10', '{"items": [{"sku": "a", "qty": 2}]}');

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

INSERT INTO r7k VALUES (1, 1, 50, 'note 1', 0.5, -0, '2025-03-09 06:59:59+00'), (2, 1, 123, 'NOTE 3', -12.125, 0.5, NULL), (3, 1, -8, '', -12.125, 1e300, '2025-03-09 07:00:00+00'), (4, 1, -48, 'note_4', 0, 'Infinity', '2025-03-09 07:00:00+00'), (5, 1, 108, 'note_4', 1000, NULL, '2025-03-09 07:00:00+00'), (6, 1, 125, 'é', -12.125, 'NaN', '2025-03-09 06:59:59+00'), (7, 1, 187, 'note 2', 1000, 'NaN', '2025-11-02 05:30:00+00'), (8, 3, 71, 'NOTE 3', 0.5, NULL, '2025-03-09 07:00:00+00'), (9, 1, -16, 'note 2', 0.5, 'NaN', '2025-03-09 07:00:00+00'), (10, 2, 17, 'note 2', 0.5, 'NaN', NULL), (11, 1, NULL, '', 3.333, 0.5, '2024-02-29 12:00:00+00'), (12, 6, NULL, 'note 2', 3.333, 1e300, '2024-02-29 12:00:00+00'), (13, 1, 109, 'é', 0, 'NaN', '2025-03-09 06:59:59+00'), (14, 1, NULL, 'note 3', NULL, NULL, '2024-02-29 12:00:00+00'), (15, 1, 61, 'Note 3', 0, 'NaN', '2025-03-09 07:00:00+00'), (16, 4, -24, 'note 1', 3.333, 2.25, '2025-11-02 05:30:00+00'), (17, 1, -17, 'NOTE 3', 3.333, -0, NULL), (18, 1, 165, 'note 2', -12.125, NULL, '2025-11-02 05:30:00+00'), (19, 1, 196, 'note 1', NULL, 'NaN', NULL), (20, 1, 23, 'note 1', 1000, 2.25, '2025-03-09 07:00:00+00'), (21, 1, 95, 'NOTE 3', -12.125, 'NaN', NULL), (22, 3, -12, 'é', -12.125, 2.25, '2024-02-29 12:00:00+00'), (23, 1, 25, 'note_4', 3.333, 0.5, NULL), (24, 1, 191, 'n%5', 3.333, 'NaN', '2025-11-02 05:30:00+00'), (25, 6, 84, 'note_4', 3.333, 2.25, '2025-03-09 06:59:59+00'), (26, 5, NULL, 'n%5', 1000, NULL, NULL), (27, 7, 50, 'Note 3', 0, 'Infinity', '2025-03-09 06:59:59+00'), (28, 1, 114, 'NOTE 3', -12.125, NULL, '2025-03-09 06:59:59+00'), (29, 1, 177, 'é', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (30, 1, -30, 'note_4', NULL, 'NaN', '2024-02-29 12:00:00+00'), (31, 1, 21, 'é', 1000, 2.25, '2024-02-29 12:00:00+00'), (32, 7, 81, '', 1000, 'NaN', '2025-11-02 05:30:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5);

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

INSERT INTO r7t VALUES (1, 'A', 'A'), (2, 'A', 'A'), (3, 'ss', 'ss'), (4, 'a', 'a'), (5, 'ss', 'ss');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

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

INSERT INTO r8a VALUES (1, 1, 6, 0, 'ab', '2025-01-01'::timestamp + interval '138 hours'), (2, 1, 3, -2.25, '', '2025-01-01'::timestamp + interval '37 hours'), (3, 1, -2, 10, '', '2025-01-01'::timestamp + interval '9 hours'), (4, 4, 10, 1.50, 'z', '2025-01-01'::timestamp + interval '98 hours'), (5, 2, NULL, 1.50, 'b', '2025-01-01'::timestamp + interval '54 hours'), (6, 2, -5, 3.75, 'B', '2025-01-01'::timestamp + interval '58 hours'), (7, 1, 10, 1.50, 'a', '2025-01-01'::timestamp + interval '33 hours'), (8, 2, 4, -2.25, 'ab', '2025-01-01'::timestamp + interval '32 hours'), (9, 1, 20, 3.75, 'z', '2025-01-01'::timestamp + interval '6 hours'), (10, 1, NULL, -2.25, 'B', '2025-01-01'::timestamp + interval '24 hours'), (11, 2, 18, 10, 'z', '2025-01-01'::timestamp + interval '78 hours'), (12, 2, -4, 3.75, 'ab', '2025-01-01'::timestamp + interval '10 hours'), (13, 4, 13, 10, 'ab', '2025-01-01'::timestamp + interval '69 hours'), (14, 1, 14, 0, 'z', '2025-01-01'::timestamp + interval '47 hours'), (15, 3, 16, NULL, '', '2025-01-01'::timestamp + interval '82 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 5, 5), (3, 3, 3), (4, 3, 8), (4, 2, 1), (5, 6, 3), (5, 7, 3), (6, 8, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{}', '[2025-01-01,2025-02-01)', NULL), (2, '(2,7]', '{}', '[2025-03-01,infinity)', '[0,0]'), (3, '[3,3)', '{}', 'empty', NULL), (4, NULL, '{}', 'empty', '(1.25,1.5)'), (5, '[0,)', '{[0,2), [4, 6)}', 'empty', '(1.25,1.5)'), (6, '(-2,4]', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (7, 'empty', '{}', '[2025-03-01,infinity)', '(,0]'), (8, NULL, '{}', 'empty', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'quick quick slow'), (2, ''), (3, 'running rats'), (4, 'foxes and dogs'), (5, 'foxes and dogs'), (6, 'jumps over the lazy dog');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '"scalar"'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (6, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3927) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1862) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2689) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r9a5700 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a5700 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 792, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 792) i;

ANALYZE (SKIP_LOCKED) r9a5700;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a5700 a JOIN r9a5700 b ON a.g = b.g WHERE a.u < 10$$);

DROP TABLE r9a5700;

CREATE FUNCTION r8f62747(lo int, hi int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE n bigint; r text := ''; got int;
BEGIN
  UPDATE r8a SET v = v WHERE id BETWEEN lo AND hi;
  GET DIAGNOSTICS n = ROW_COUNT;
  r := r || 'upd=' || n || ' found=' || FOUND;
  SELECT id INTO STRICT got FROM r8a WHERE id BETWEEN lo AND hi ORDER BY id;
  r := r || ' got=' || coalesce(got::text, 'null') || ' found=' || FOUND;
  PERFORM 1 FROM r8a WHERE false;
  r := r || ' perform=' || FOUND;
  RETURN r;
END $$;

SELECT r8f62747(1, 1);

SELECT r8f62747(1, 3);

SELECT r8f62747(1000, 1001);

DROP FUNCTION r8f62747;

CREATE TABLE r9a11837 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a11837 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 201, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 201) i;

ALTER TABLE r9a11837 ALTER COLUMN arr SET STATISTICS 1;

UPDATE r9a11837 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a11837 (g, s, arr);

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a11837 WHERE arr @> '{2}'$$);
