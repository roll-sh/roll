-- class: engine_error
-- ordered: false
-- note: 26000: prepared statement "r9p80975" does not exist
-- engine: ERROR 26000: prepared statement "r9p80975" does not exist
-- reference: id:23 | x:1700
-- reference: 1499 | 214.1429
-- reference: -- SELECT 1

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a json, b text);

INSERT INTO t0 (id, a, b) VALUES (18, '{"a": "1"}', 'Ab'), (25, '{"a": {"b": 2}}', 'ß'), (5, '[1, 2, 3]', ''), (34, '[1, 2, 3]', 'ß'), (35, '"s"', 'Ä');

CREATE INDEX t0_i649 ON t0 (b) WHERE b IS NOT NULL;

CREATE INDEX t0_i167 ON t0 (b DESC NULLS LAST);

CREATE INDEX t0_i431 ON t0 (id) WHERE id IS NOT NULL;

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (a json, b bool);

INSERT INTO t1 (a, b) VALUES ('{"a": 3}', NULL), (NULL, true), ('{"a": 3}', true), ('{"b": [1, "a"]}', NULL), ('{"a": "1"}', false), ('1', false), ('{"a": 3}', true), (NULL, true), (NULL, true), (NULL, true), ('{"a": 1, "b": "x"}', true), ('1', NULL), (NULL, NULL), ('{"a": {"b": 2}}', NULL), ('"s"', NULL), ('null', false), (NULL, NULL), ('[1, 2, 3]', true), (NULL, NULL), ('{"a": 1, "b": "x"}', NULL), ('{"a": "1"}', false), ('{"b": [1, "a"]}', true), (NULL, NULL), ('1', false), (NULL, false), (NULL, NULL), ('"s"', false), (NULL, NULL), ('null', NULL), ('1', NULL);

CREATE INDEX t1_i505 ON t1 (b);

CREATE TABLE t2 (id int4 PRIMARY KEY, a int8, b timestamptz, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t2 (id, a, b) VALUES (20, 11, NULL), (7, NULL, NULL), (35, 4, '2024-01-06 23:00:00+00');

CREATE INDEX t2_i569 ON t2 (b) WHERE b IS NOT NULL;

CREATE INDEX t2_i305 ON t2 (b);

CREATE TYPE ct3 AS (a text COLLATE "C", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, NULL, NULL, NULL), (2, ROW('a%', 1)::ct3, NULL, 1), (3, ROW('a_b', -1)::ct3, ARRAY[ROW(' x', 3)::ct3, ROW('a,b', NULL)::ct3], 2), (4, NULL, '{}', NULL), (5, ROW('a', 2)::ct3, '{}', NULL), (6, NULL, NULL, 4), (7, ROW('foo', -1)::ct3, ARRAY[ROW('a_b', 0)::ct3, ROW('Ab', -1)::ct3], NULL);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT CASE WHEN true THEN p ELSE (jsonb_array_length('{"a": 2.50}'::jsonb) & p) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql STABLE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN isfinite('2024-12-23'::date) THEN RETURN (NULLIF(9223372036854775807::int8, 11::int8))::int4;
  ELSE RETURN position(concat(q, p) in left(' x', 0));
  END CASE;
END
$$;

CREATE VIEW v0 AS SELECT DISTINCT '2000-11-18 10:59:00'::timestamp AS c0 FROM (((SELECT md5(concat(CASE WHEN true THEN NULL::text ELSE ' x' END, COALESCE(6, NULL::int4))) AS c0, true AS c1, NULLIF(abs((-5)::int8), (6::int8)::text::int8) > NULL::int8 AS c2 FROM t1 t2 WHERE false) UNION (SELECT (to_jsonb(lpad('bar', 3, '*')) ->> 'b') AS c0, 9 BETWEEN extract(isodow from '2000-12-09 09:00:00+00'::timestamptz)::int AND sign(0)::int AS c1, regexp_replace('b', '[ab]', 'y', 'g') NOT BETWEEN 'bar' AND initcap('Ä') AS c2 FROM t1 t3 LEFT JOIN t1 t4 USING (b))) s1 JOIN t2 t5 ON NULL::bool) WHERE (s1.c2 OR (jsonb_array_length('{"a": 2.50}'::jsonb), ('null'::jsonb ->> 'b')) = (width_bucket('NaN'::numeric, 0, 10, 4), s1.c0));

CREATE VIEW v1 AS (SELECT every((isfinite('2024-03-11'::date) AND NULL::bool)) AS c0 FROM t2 t1 GROUP BY GROUPING SETS ((t1.a), ((t1.g))) ORDER BY 1 NULLS FIRST LIMIT 6) EXCEPT (SELECT ('"s"'::jsonb::jsonb ->> 'a')::int <> ALL (ARRAY[extract(isodow from '2000-03-27 19:00:00-05'::timestamptz)::int, t2.id]) AS c0 FROM (t0 t2 LEFT JOIN v0 t3 ON LEAST(t2.b, 'a_b') < t2.b COLLATE "default"));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '1970-01-01 00:00:00.5', NULL, '1 mon -30 days', '-1e-20', '2', -2), (2, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', NULL, '4713-01-01 BC', '36 hours', '-2.5', '-0', 3), (3, '"str"', '"str"', '2025-03-09 01:30:00-05', '1970-01-01 00:00:00.5', '2025-01-01', NULL, '-7', '1024', -3), (4, '"str"', '"str"', '2025-11-02 01:30:00-05', 'infinity', '2025-01-01', 'P1DT12H', '-Infinity', 'NaN', 0), (5, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '2025-04-06 02:30:00+11', '1970-01-01 00:00:00.5', '2025-03-09', '2 weeks 3 microseconds', '0', NULL, 5);

INSERT INTO tp4 VALUES (16, '2000-03-01', 'd', '2.5'), (1, '2025-01-01', 'ab', '0.000001'), (15, '2024-02-29', 'd', 'NaN'), (14, '2025-03-09', 'a', 'NaN'), (22, '2025-03-09', 'a', '1e30'), (19, NULL, 'd', '99999999999999999999'), (6, '4713-01-01 BC', 'ab', '2.5'), (11, NULL, 'a', '0.000001'), (-5, '2000-03-01', 'b', '1.23456789012345678901234567890'), (6, NULL, NULL, 'NaN'), (9, '2024-02-29', 'c', '1e30'), (24, '-infinity', 'a', '0.000001'), (4, '4713-01-01 BC', 'b', '0.000001'), (21, NULL, 'c', '-1e-20'), (-5, '2024-02-29', 'd', '2.5'), (6, '2024-02-29', 'a', '2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 16, 'ı', '-0.5', '1 day 00:00:00.000001', '(,2]', '{}', 'Fat cats! FAT rats? fat-free', '{}'), (2, 16, 'e', '0.0000000000000000000000000001', '2147483647 mons', '[0,10)', '{[1,3),[5,7)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(NULL, 'a-')::r5ct, ROW(-2, 'z')::r5ct]), (3, 15, NULL, NULL, '1 year -1 sec', '[-3,0]', '{[2,4)}', 'running runners ran; the runner runs', ARRAY[ROW(4, 'AB')::r5ct, ROW(NULL, 'Z')::r5ct]);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 3 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 8 WITH CHECK OPTION;

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

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (10, 'a', '2024-02-29'), (12, 'b', NULL), (17, 'c', 'infinity'), (16, 'd', 'infinity'), (4, 'a', NULL), (2, 'b', '2024-02-29'), (1, 'c', '2024-02-29') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (29, 'x0', NULL), (30, 'x1', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob1', 1, '{}', '{"seats": 1.50}'), (2, 'u2@x.org', 'Éva2', 2, '{a}', '{}'), (3, 'u3@x.org', 'ann3', 3, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 9.99, 'new', '2025-02-06', '{"items": []}'), (2, 12345678.90, 'new', '2025-02-14', NULL), (1, 12345678.90, 'paid', '2025-02-17', '{"gift": true}'), (2, 12345678.90, 'paid', '2025-02-03', NULL), (3, -1.50, 'paid', '2025-02-11', '{"gift": true}'), (2, -1.50, 'paid', '2025-02-17', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 100, 'paid', '2025-02-17', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 2, 178, 'note_4', 0, 'Infinity', '2025-03-09 07:00:00+00'), (2, 1, -22, 'note 3', 0.5, -0, '2025-03-09 06:59:59+00'), (3, 6, 119, 'n%5', 3.333, 'NaN', '2024-02-29 12:00:00+00'), (4, 1, 124, 'note_4', 0.5, 2.25, '2024-02-29 12:00:00+00'), (5, 1, -24, 'n%5', 3.333, -0, '2025-03-09 07:00:00+00'), (6, 1, 127, 'note 1', 0, 1e300, '2025-11-02 05:30:00+00'), (7, 1, -33, 'note 1', 0.5, 2.25, '2025-11-02 05:30:00+00'), (8, 1, 134, 'note 1', 0.5, 2.25, '2025-11-02 05:30:00+00'), (9, 1, 74, 'Note 3', 3.333, 'Infinity', NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12);

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

INSERT INTO r7t VALUES (1, 'ä', 'ä'), (2, 'ä', 'ä'), (3, 'ss', 'ss'), (4, 'E', 'E'), (5, 'Æ', 'Æ'), (6, 'ä b', 'ä b'), (7, '9', '9'), (8, 'Ä', 'Ä'), (9, 'Æ', 'Æ'), (10, 'e', 'e'), (11, 'b', 'b'), (12, 'z', 'z'), (13, 'ss', 'ss');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 1, 10, 1.50, 'ab', '2025-01-01'::timestamp + interval '30 hours'), (2, 4, 3, -2.25, 'B', '2025-01-01'::timestamp + interval '57 hours'), (3, 2, -5, 10, 'z', '2025-01-01'::timestamp + interval '138 hours'), (4, 3, 1, NULL, 'a', '2025-01-01'::timestamp + interval '22 hours'), (5, 4, 12, NULL, 'ab', '2025-01-01'::timestamp + interval '24 hours'), (6, 2, 8, NULL, 'ab', '2025-01-01'::timestamp + interval '102 hours'), (7, 1, 16, 3.75, 'a', '2025-01-01'::timestamp + interval '36 hours'), (8, 1, 5, 10, 'ab', '2025-01-01'::timestamp + interval '28 hours'), (9, 1, 20, 1.50, '', '2025-01-01'::timestamp + interval '67 hours'), (10, 1, 12, NULL, 'ab', '2025-01-01'::timestamp + interval '56 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 3, 8), (4, 1, 3), (5, 4, 1), (6, 5, 2), (6, 1, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{(,2)}', 'empty', '[0,0]'), (2, '(-3,3]', '{[-3,3), [5, 7)}', '[2025-03-01,infinity)', '[1.5,2.5)'), (3, '[0,)', '{}', 'empty', '(1.25,1.5)'), (4, '[3,3)', '{(,3)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (5, NULL, '{(,4)}', '[2025-01-01,2025-02-01)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, 'running rats'), (3, 'jumps over the lazy dog'), (4, 'über café naïve'), (5, 'running rats'), (6, 'jumps over the lazy dog'), (7, 'the quick brown fox'), (8, 'running rats'), (9, 'jumps over the lazy dog'), (10, 'foxes and dogs');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '"scalar"'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2023) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2800) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2623) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = auto; FOR i IN 0..12 LOOP s := s + coalesce((SELECT count(*) FROM r9s WHERE g = i % 4 + 1), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

CREATE TABLE r9i36410 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i36410 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 181) i;

ANALYZE r9i36410;

CREATE INDEX r9i36410_x0 ON r9i36410 USING btree (t COLLATE "C");

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i36410 WHERE a BETWEEN 10 AND 20;

EXPLAIN (COSTS OFF) SELECT id, a FROM r9i36410 WHERE a < 12 ORDER BY a DESC NULLS FIRST, id LIMIT 10;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i36410;

SELECT sum(v) OVER (ORDER BY g ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM r8a;

SET plan_cache_mode = force_generic_plan;

PREPARE r9p38494 AS SELECT CASE WHEN $1 > 1 THEN 'many' ELSE (SELECT max(t) FROM r9s WHERE g = $1) END;

EXECUTE r9p38494(5);

EXECUTE r9p38494(8);

EXECUTE r9p38494(1);

EXECUTE r9p38494(3);

EXECUTE r9p38494(1);

EXPLAIN (COSTS OFF) EXECUTE r9p38494(1);

EXECUTE r9p38494(1);

EXECUTE r9p38494(1);

EXECUTE r9p38494(2);

EXECUTE r9p38494(5);

EXECUTE r9p38494(99);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p38494';

DEALLOCATE r9p38494;

RESET plan_cache_mode;

\s2 LISTEN r8ch59105;

\s3 LISTEN r8ch59105;

NOTIFY r8ch59105, 'from s1';

\s2 SELECT 1;

\s3 BEGIN;

\s3 NOTIFY r8ch59105, 'a';

\s3 NOTIFY r8ch59105, 'a';

\s3 SELECT pg_notify('r8ch59105', 'b');

\s3 COMMIT;

\s2 SELECT 2;

\s3 SELECT pg_listening_channels();

\s2 SELECT pg_listening_channels(), pg_notification_queue_usage() < 0.5;

\s2 UNLISTEN r8nope59105;

NOTIFY r8ch59105, 'after unlisten';

\s2 SELECT 3;

SELECT pg_notify(repeat('c', 64), 'x');

\s3 UNLISTEN *;

\s2 UNLISTEN *;

WITH RECURSIVE r(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + 2) % 4, x || n::text FROM r WHERE length(x) < 4) SEARCH DEPTH FIRST BY n SET ord SELECT n, x, ord FROM r;

SET plan_cache_mode = force_generic_plan;

PREPARE r9p80975 AS SELECT id, x FROM r9s WHERE id BETWEEN $1 AND $1 + $2 ORDER BY id DESC LIMIT 20;

-- statement
EXECUTE r9p80975(1499, 0);
