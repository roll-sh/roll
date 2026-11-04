-- class: wrong_rows
-- ordered: false
-- note: 1 vs 1 rows
-- engine: name:25 | generic_plans:20 | custom_plans:20 | from_sql:16 | parameter_types:25 | result_types:25
-- engine: r9x67914 | 8 | 6 | f | {text} | {integer,integer}
-- engine: -- SELECT 1 [I]
-- reference: name:25 | generic_plans:20 | custom_plans:20 | from_sql:16 | parameter_types:25 | result_types:25
-- reference: r9x67914 | 9 | 5 | f | {text} | {integer,integer}
-- reference: -- SELECT 1 [I]

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a int4, b bool, c int8, d date) PARTITION BY RANGE (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO (3);

CREATE TABLE t0_p1 PARTITION OF t0 FOR VALUES FROM (3) TO (6);

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

CREATE INDEX t0_i31 ON t0 (d, c);

CREATE INDEX t0_i757 ON t0 (c);

CREATE INDEX t0_i242 ON t0 (c);

CREATE TABLE t1 (id int4, a bool, b date, c text, d timestamptz) PARTITION BY RANGE (b);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES FROM (MINVALUE) TO ('2024-02-01');

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

INSERT INTO t1 (id, a, b, c, d) VALUES (9, true, '2024-02-22', 'ß', NULL), (24, false, NULL, 'Ä', '2024-01-01 06:00:00+00'), (2, false, '2024-02-22', 'B', '2024-01-09 06:00:00+00'), (27, NULL, '2024-01-08', NULL, NULL), (10, false, NULL, 'Ab', NULL), (6, true, '2024-03-08', '9', '2024-01-01 00:00:00+00'), (37, true, '2024-03-20', 'Ä', '2024-01-09 00:00:00+00'), (32, false, NULL, NULL, '2024-01-05 12:00:00+00'), (39, true, '2024-01-11', 'ab', '2024-01-03 06:00:00+00'), (35, false, '2024-01-11', NULL, NULL), (4, NULL, '2024-02-03', NULL, '2024-01-02 06:00:00+00'), (28, false, '2024-02-14', 'ba', NULL), (17, false, NULL, NULL, '2024-01-08 12:00:00+00'), (19, NULL, NULL, 'b', '2024-01-08 00:00:00+00'), (43, false, '2024-01-26', 'x ', NULL), (41, NULL, NULL, 'Z', '2024-01-06 00:00:00+00'), (33, NULL, '2024-01-14', 'ba', '2024-01-03 23:00:00+00'), (36, NULL, '2024-02-11', 'z', NULL), (16, false, NULL, NULL, '2024-01-04 23:00:00+00'), (44, true, '2024-01-25', 'bar', '2024-01-09 06:00:00+00'), (31, NULL, NULL, '9', '2024-01-02 23:00:00+00'), (26, false, '2024-03-15', 'B', '2024-01-02 06:00:00+00'), (40, NULL, '2024-03-12', 'x ', '2024-01-05 12:00:00+00'), (3, NULL, '2024-02-24', NULL, NULL), (38, NULL, NULL, 'foo', NULL), (7, true, '2024-03-22', NULL, '2024-01-07 00:00:00+00'), (11, false, NULL, NULL, '2024-01-08 06:00:00+00'), (13, NULL, '2024-03-24', 'ß', '2024-01-04 23:00:00+00'), (5, false, NULL, ' x', NULL), (22, false, NULL, 'é', '2024-01-07 06:00:00+00');

CREATE INDEX t1_i437 ON t1 USING hash (id);

CREATE INDEX t1_i553 ON t1 (b);

CREATE TABLE t2 (id int4 PRIMARY KEY, a interval, b numeric(10,2), c json, d bool, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t2 (id, a, b, c, d) VALUES (9, '0', -1.00, NULL, false), (34, NULL, NULL, '"s"', true), (22, '-1 day', 0.00, '{"a": 1, "b": "x"}', false);

CREATE INDEX t2_i748 ON t2 USING hash (g);

CREATE TABLE t3 (id int4 PRIMARY KEY, a int4[], b float8);

INSERT INTO t3 (id, a, b) VALUES (40, '{5}', NULL), (11, '{2}', NULL), (14, '{NULL,3}', NULL), (32, NULL, NULL), (17, NULL, NULL);

CREATE INDEX t3_i214 ON t3 USING gin (a);

CREATE INDEX t3_i724 ON t3 (a);

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE sql STABLE AS $$ SELECT id FROM t2 WHERE id <= lim $$;

CREATE VIEW v0 AS SELECT array_agg(substr(regexp_replace('A', '[ab]', 'y', 'g'), 1, 3) ORDER BY substr(regexp_replace('A', '[ab]', 'y', 'g'), 1, 3)) FILTER (WHERE t1.g IN (WITH w2 AS MATERIALIZED (SELECT (COALESCE('{A,a}'::text[], ARRAY[t3.c, t3.c]))::text::text[] AS c0 FROM (t1 t3 LEFT JOIN t0 t4 ON t3.id = t4.id AND (t3.c || 'Ab') ILIKE '%b')), w5 AS NOT MATERIALIZED (SELECT array_append(string_to_array('a', ','), trim(('{"a": 1, "b": "x"}'::jsonb ->> 'b'))) AS c0, GREATEST(t6.c, t6.c) AS c1, (t6.d - t6.id) AS c2, lower((daterange(LEAST(COALESCE(t6.d, (t6.d + t1.id)), t6.d), GREATEST(COALESCE(t6.d, (t6.d + t1.id)), t6.d)) * daterange(NULL, t6.d))) AS c3 FROM t0 t6 WHERE t6.b) SELECT 1 AS c0 FROM w5 t7)) AS c0 FROM t2 t1 WHERE (int4range(LEAST(((NULL::int4[] || '{}'::int4[]))[1], cardinality((NULL::int4[])[1:2])), GREATEST(((NULL::int4[] || '{}'::int4[]))[1], cardinality((NULL::int4[])[1:2]))) && int4range(LEAST(7, 12), GREATEST(7, 12))) GROUP BY t1.g;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '"str"', '"str"', '1999-12-31 23:59:59.999999+00', '2000-01-01 00:00:00', NULL, '23:59:59.999999', '1.23456789012345678901234567890', '-1.25', 6), (2, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '1999-12-31 23:59:59.999999+00', '2025-10-26 02:30:00', '2025-01-01', '2 weeks 3 microseconds', NULL, '0.125', 3), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-11-02 01:30:00-04', '2000-01-01 00:00:00', '4713-01-01 BC', '1 mon -30 days', '1.23456789012345678901234567890', '1024', NULL), (4, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '2025-03-09 01:30:00-05', '2025-10-26 02:30:00', '-infinity', '1 day', '1.23456789012345678901234567890', 'NaN', NULL), (5, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '2025-03-09 02:30:00', '4713-01-01 BC', '0', '-7', '1024', 3), (6, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', NULL, '2025-12-31 23:59:59.999999', '-infinity', '-infinity', NULL, '0.5', 0);

INSERT INTO tp4 VALUES (18, '2024-02-29', 'd', '-2.5'), (0, '-infinity', 'd', '-Infinity'), (2, '2000-03-01', 'd', '1.23456789012345678901234567890'), (7, '2025-01-01', 'a', '99999999999999999999'), (10, 'infinity', 'd', '1.23456789012345678901234567890'), (13, '2025-01-01', 'd', 'Infinity'), (12, 'infinity', 'a', '-7'), (25, '2000-03-01', 'c', '0'), (7, 'infinity', 'd', '0'), (3, '2024-02-29', 'b', '-0.0'), (8, '2025-03-09', 'd', '-0.0'), (3, '2025-03-09', 'a', '-2.5'), (21, 'infinity', NULL, '99999999999999999999'), (12, '2025-03-09', 'c', 'Infinity');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "tr-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 16, 'Ab', NULL, '2147483647 days', '[0,10)', '{empty}', 'running runners ran; the runner runs', NULL), (2, 11, 'a', '2.5', '-2147483648 days', '(,)', '{[1,2],[2,3)}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (3, NULL, 'i', 'NaN', '1 year -1 sec', '[3,)', '{empty}', 'PostgreSQL full text search: lexemes & weights', NULL), (4, NULL, 'A', '0', '0', '(,2]', NULL, NULL, '{}'), (5, 6, NULL, '-0.5', 'infinity', '[3,)', '{[2,4)}', 'running runners ran; the runner runs', ARRAY[ROW(-1, 'ä')::r5ct, ROW(4, 'ö')::r5ct]), (6, 12, 'ı', '-99999999999999999999999999999999999.999999', '1 year -1 sec', '[3,)', NULL, 'running runners ran; the runner runs', ARRAY[ROW(5, ' a')::r5ct, ROW(3, 'b')::r5ct, ROW(1, 'ä')::r5ct]), (7, 12, ' a', NULL, '-2147483648 days', '[-3,0]', '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', NULL), (8, 4, '', '0.0000000000000000000000000001', 'infinity', NULL, NULL, '', ARRAY[ROW(-1, 'AB')::r5ct, ROW(-1, 'İ')::r5ct]), (9, -3, 'ö', '123456789012345678901234567890.123456789', '1 mon', '[1,5)', '{empty}', 'running runners ran; the runner runs', ARRAY[ROW(1, 'ab')::r5ct, ROW(NULL, 'ae')::r5ct, ROW(5, '')::r5ct]);

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 23;

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

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (11, 'a', 'infinity'), (-5, 'b', 'infinity'), (6, 'c', '2025-01-01'), (2, 'd', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (20, 'x0', NULL), (26, 'x1', NULL), (32, 'x2', NULL), (20, 'x3', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, NULL, '{"seats": 1.50}'), (2, 'u2@x.org', 'Éva2', 0, '{}', '{"plan": "free", "tags": [1, 2]}'), (3, 'u3@x.org', 'zoë3', 3, '{}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', 'ann4', 4, '{}', '{}'), (5, 'u5@x.org', 'zoë5', 0, NULL, '{"plan": "pro", "seats": 3}'), (6, 'u6@x.org', 'bob 6', 6, '{a}', NULL);

INSERT INTO r6o (uid, amount, status, created, data) VALUES (4, -1.50, 'paid', '2025-02-10', '{"gift": true}'), (5, 12345678.90, 'paid', '2025-02-06', NULL), (1, 9.99, 'new', '2025-02-23', NULL), (4, -1.50, 'void', '2025-02-25', '{"items": []}'), (4, 0, 'new', '2025-02-14', '{"items": [{"sku": "a", "qty": 2}]}'), (4, -1.50, 'new', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 9.99, 'paid', '2025-02-12', '{"items": []}'), (5, -1.50, 'paid', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 9.99, 'void', '2025-02-01', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 9.99, 'paid', '2025-02-05', NULL), (3, 9.99, 'new', '2025-02-24', NULL), (6, 12345678.90, 'paid', '2025-02-27', '{"items": [{"sku": "a", "qty": 2}]}'), (6, NULL, 'void', '2025-02-13', '{"items": []}'), (6, -1.50, 'new', '2025-02-27', '{"gift": true}'), (2, 0, 'paid', '2025-02-13', '{"items": [{"sku": "a", "qty": 2}]}'), (4, NULL, 'paid', '2025-02-13', '{"items": []}'), (1, NULL, 'void', '2025-02-06', '{"items": []}'), (2, 100, 'new', '2025-02-16', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, 48, 'Note 3', 0.5, 2.25, NULL), (2, 9, -45, 'é', 3.333, 1e300, '2024-02-29 12:00:00+00'), (3, 1, 189, 'note_4', NULL, 'Infinity', NULL), (4, 7, 6, 'n%5', 1000, 2.25, '2024-02-29 12:00:00+00'), (5, 1, -13, 'note 2', 1000, 2.25, '2024-02-29 12:00:00+00'), (6, 1, 114, 'note 1', 0, 1e300, '2025-03-09 07:00:00+00'), (7, 7, 12, 'é', NULL, -0, '2025-11-02 05:30:00+00'), (8, 1, 178, 'note 2', NULL, -0, NULL), (9, 1, 9, 'é', -12.125, -0, NULL), (10, 1, 6, 'note 3', 0, -0, '2024-02-29 12:00:00+00'), (11, 1, 78, 'é', 0.5, -0, '2025-03-09 07:00:00+00'), (12, 9, NULL, 'NOTE 3', -12.125, 1e300, NULL), (13, 1, 164, '', 1000, 0.5, '2025-11-02 05:30:00+00'), (14, 1, 156, 'NOTE 3', 0, 2.25, '2025-03-09 06:59:59+00'), (15, 4, NULL, 'Note 3', NULL, 0.5, '2025-03-09 06:59:59+00'), (16, 7, 143, 'note 3', 3.333, NULL, NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

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

INSERT INTO r7t VALUES (1, 'ä', 'ä'), (2, 'Æ', 'Æ'), (3, '', ''), (4, 'ae', 'ae'), (5, 'z', 'z'), (6, 'é', 'é'), (7, 'z', 'z'), (8, 'b', 'b'), (9, 'ae', 'ae'), (10, 'ß', 'ß');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, 12, 1.50, 'ab', '2025-01-01'::timestamp + interval '0 hours'), (2, 3, 11, NULL, '', '2025-01-01'::timestamp + interval '24 hours'), (3, 2, 8, 0, '', '2025-01-01'::timestamp + interval '198 hours'), (4, 2, 9, NULL, 'a', '2025-01-01'::timestamp + interval '60 hours'), (5, 4, -4, 3.75, 'a', '2025-01-01'::timestamp + interval '41 hours'), (6, 4, NULL, NULL, 'b', '2025-01-01'::timestamp + interval '105 hours'), (7, 3, 15, 10, 'ab', '2025-01-01'::timestamp + interval '58 hours'), (8, 4, 1, 10, 'b', '2025-01-01'::timestamp + interval '24 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 5), (1, 2, 2), (2, 4, 5), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[3,5)', '{(,3)}', '[2025-01-01,2025-02-01)', '[0,0]'), (2, '[2,)', NULL, NULL, '[1.5,2.5)'), (3, 'empty', '{}', NULL, '[0,0]'), (4, '[5,)', NULL, '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (5, '(2,6]', '{}', '[2025-01-01,2025-02-01)', NULL), (6, 'empty', '{[6,10), [12, 14)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (7, '[-3,3)', '{[-3,3), [5, 7)}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (8, '[8,9)', NULL, NULL, NULL), (9, '[6,12)', '{[6,12), [14, 16)}', NULL, '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, 'dog-eat-dog world'), (3, 'running rats'), (4, 'brown''s fox''s den'), (5, 'a rat ran'), (6, 'a rat ran'), (7, 'the quick brown fox');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2559) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1865) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3179) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

\x2 name=r9x67914 types=25 describe rf=0 p=b7b35302c3130302c4e554c4c7d
SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;

\x2 name=r9x67914 rf=1,0 p=t'{}'
-;

\x2 name=r9x67914 rows=2 p=t'{50,100,NULL}'
-;

\x2 name=r9x67914 rf=1,0 p=t'{50,100,NULL}'
-;

\x2 name=r9x67914 rf=1,0 p=b7b7d
-;

\x2 name=r9x67914 p=t'{50,100,NULL}'
-;

\s2 ANALYZE r9s;

\x2 name=r9x67914 rf=1 p=b7b323939392c317d
-;

\x2 name=r9x67914 rf=1 p=b7b7d
-;

\s2 DISCARD PLANS;

\x2 name=r9x67914 rf=1,0 p=t'{1,2,3}'
-;

\s2 SET plan_cache_mode = auto;

\x2 name=r9x67914 rf=1,0 p=b7b323939392c317d
-;

\x2 name=r9x67914 rf=1,0 p=t'{2999,1}'
-;

\x2 name=r9x67914 rows=1 p=t'{2999,1}'
-;

\x2 name=r9x67914 rf=1,0 p=t'{50,100,NULL}'
-;

\x2 name=r9x67914 rows=1 p=t'{50,100,NULL}'
-;

-- statement
\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x67914';
