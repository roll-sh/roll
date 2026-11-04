-- class: wrong_rows
-- ordered: false
-- note: 13 vs 10 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: r9i77220.a DESC, r9i77220.id
-- engine:         ->  Sort
-- engine:               Sort Key: r9i77220_1.a DESC, r9i77220_1.id
-- engine:               ->  Seq Scan on r9i77220_1
-- engine:                     Disabled: true
-- engine:                     Filter: (a < 41)
-- engine:         ->  Sort
-- engine:               Sort Key: r9i77220_2.a DESC, r9i77220_2.id
-- engine:               ->  Seq Scan on r9i77220_2
-- engine:                     Disabled: true
-- engine:                     Filter: (a < 41)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: r9i77220.a DESC, r9i77220.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i77220_1
-- reference:                     Disabled: true
-- reference:                     Filter: (a < 41)
-- reference:               ->  Seq Scan on r9i77220_2
-- reference:                     Disabled: true
-- reference:                     Filter: (a < 41)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a json, b timestamp, c d3int, d float8, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

CREATE TABLE t0_c (extra int) INHERITS (t0);

INSERT INTO t0_c (id, a, b, c, d, extra) VALUES (338, '{"a": 1, "b": "x"}', '2024-01-01 23:00:00', 7, 1.5, 2);

INSERT INTO t0 (id, a, b, c, d) VALUES (35, '{"a": 3}', '2024-01-05 00:30:00', NULL, NULL), (5, '1', NULL, 2, 3.75), (22, '[1, 2, 3]', NULL, NULL, -0.25), (30, '{"a": 1, "b": "x"}', '2024-01-04 00:30:00', NULL, -1.75), (37, NULL, '2024-01-02 06:00:00', 4, 1), (1, 'null', '2024-01-04 06:00:00', 2, -0.25), (34, '{"a": "1"}', '2024-01-04 12:30:00', -1, -1.75), (38, '[1, 2, 3]', '2024-01-02 23:00:00', 1, -1.5), (20, '{"a": 3}', '2024-01-02 23:30:00', NULL, 1.75), (8, '{"a": "1"}', NULL, 1, 3.5), (33, NULL, '2024-01-09 23:30:00', 3, -0.75), (12, '{"a": {"b": 2}}', NULL, 2, 2), (54, '[]', '2024-01-04 12:30:00', 5, 3.25), (23, '{"a": "1"}', NULL, 9, -0.25), (2, '[1, 2, 3]', '2024-01-02 00:30:00', -2, 1.75), (36, '[1, 2, 3]', '2024-01-03 12:00:00', 0, -1.25), (40, '[1, 2, 3]', '2024-01-04 00:30:00', 0, 2.75), (25, '{"a": 1, "b": "x"}', '2024-01-03 23:30:00', 7, 0.25), (50, '[]', '2024-01-07 23:00:00', 0, 3.5), (44, '{"a": "1"}', '2024-01-07 00:00:00', 3, 3.5);

CREATE INDEX t0_i256 ON t0 (id, d);

CREATE INDEX t0_i87 ON t0 (d);

CREATE TABLE t1 (a numeric(10,2), b interval);

INSERT INTO t1 (a, b) VALUES (4.75, NULL), (1.75, '30 days'), (8.50, '1 day'), (1.00, '1 mon'), (11.50, '30 days'), (9.50, '1 day'), (9.05, '30 days'), (NULL, '24:00:00'), (11.05, '1 day'), (-1.05, '2 hours'), (-2.75, '30 days'), (-1.25, '1 day'), (2.05, '1 day'), (4.00, '1 mon'), (-1.05, '0'), (8.75, '2 hours'), (10.05, '1 day'), (6.00, '24:00:00'), (11.25, '2 hours'), (4.00, NULL);

CREATE INDEX t1_i603 ON t1 USING hash (b);

CREATE INDEX t1_i922 ON t1 (b);

CREATE INDEX t1_i782 ON t1 (b) WHERE b IS NOT NULL;

CREATE TABLE t2 (id int4, a int2, b int4, c jsonb, d text, e int4[]);

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (504, 1, NULL, NULL, NULL, '{NULL,3}', 3);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (196, NULL, NULL, NULL, NULL, NULL, 1);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (449, NULL, 0, NULL, 'a', '{}', 0);

INSERT INTO t2_c (id, a, b, c, d, e, extra) VALUES (377, NULL, NULL, NULL, ' x', NULL, 2);

INSERT INTO t2 (id, a, b, c, d, e) VALUES (38, NULL, NULL, 'null', NULL, NULL);

CREATE INDEX t2_i684 ON t2 (d);

CREATE INDEX t2_i36 ON t2 (lower(d));

CREATE TABLE t3 (a float8, b timestamptz);

INSERT INTO t3 (a, b) VALUES (0.5, NULL), (NULL, NULL), (-1.75, '2024-01-06 23:00:00+00'), (NULL, NULL), (-1, '2024-01-07 23:00:00+00');

CREATE INDEX t3_i963 ON t3 (b);

CREATE INDEX t3_i206 ON t3 (b);

CREATE INDEX t3_i543 ON t3 USING hash (a);

CREATE TYPE ct3 AS (a text, b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW(NULL, 0)::ct3, NULL, 2), (2, NULL, NULL, 1), (3, ROW('a', 1)::ct3, ARRAY[ROW('a_b', 2)::ct3, ROW('é', -1)::ct3], 1), (4, NULL, ARRAY[ROW('10', 3)::ct3, ROW('x ', 2)::ct3], 4), (5, NULL, '{}', 4), (6, ROW('b', 3)::ct3, ARRAY[ROW('a,b', 2)::ct3, ROW('bar', 1)::ct3], 0);

CREATE VIEW v0 AS SELECT '[1, 2, 3]'::json AS c0, '{}'::int4[] AS c1 FROM t1 t1 WHERE NULLIF(false, true);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, NULL, NULL, '2025-11-02 01:30:00-04', '1970-01-01 00:00:00.5', '2000-03-01', '1 day', '2.5', '0.5', 5), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '0001-01-01 00:00:00 BC', '2024-02-29', NULL, '1.23456789012345678901234567890', '-1.25', -2), (3, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', NULL, '0001-01-01 00:00:00 BC', '2000-03-01', NULL, '99999999999999999999', 'Infinity', 12), (4, NULL, NULL, NULL, '0001-01-01 00:00:00 BC', '2000-03-01', '-1 mon 2 days', '1.23456789012345678901234567890', NULL, 12), (5, NULL, NULL, '0044-03-15 12:00:00+00 BC', '0001-01-01 00:00:00 BC', '2000-03-01', '-12:00', 'NaN', 'NaN', 7);

INSERT INTO tp4 VALUES (-3, NULL, 'd', NULL), (3, '1999-12-31', 'b', '-Infinity'), (13, '2000-03-01', 'd', 'Infinity');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 18, 'Ab', '2.5', '2147483647 mons', 'empty', '{[1,2],[2,3)}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (2, 14, 'e', '-0.5', '-178000000 years', '[5,6)', '{}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(3, 'é')::r5ct, ROW(3, 'o')::r5ct]), (3, 10, 'E', '2.5', NULL, 'empty', '{[2,4)}', '', NULL), (4, -1, 'Ab', '-1e40', NULL, '[3,)', '{[1,3),[5,7)}', 'Supernovae stars are the brightest phenomena in galaxies', '{}'), (5, NULL, 'Ab', '0', '-178000000 years', '[5,6)', '{[2,4)}', 'Supernovae stars are the brightest phenomena in galaxies', '{}'), (6, 19, '10', '7', '178000000 years', '(,2]', '{[1,2],[2,3)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(NULL, 'E')::r5ct, ROW(3, 'ae')::r5ct, ROW(0, '-a')::r5ct]), (7, NULL, 'o', '-99999999999999999999999999999999999.999999', '-2147483648 days', NULL, '{[0,100)}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(4, 'Ab')::r5ct, ROW(-1, 'ı')::r5ct, ROW(-1, 'o')::r5ct]), (8, 15, 'SS', '0.0000000000000000000000000001', '2147483647 days', '(1,5]', '{[1,3),[5,7)}', 'running runners ran; the runner runs', NULL), (9, 20, 'ä', '7', NULL, '[-3,0]', '{[2,4)}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (10, 9, 'A', NULL, '1 mon', '[-3,0]', '{[1,2],[2,3)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(NULL, 'ß')::r5ct, ROW(1, ' a')::r5ct, ROW(3, 'ss')::r5ct]), (11, -3, ' a', NULL, 'infinity', 'empty', '{(,0],[10,)}', NULL, ARRAY[ROW(-1, 'ä')::r5ct, ROW(-1, 'ö')::r5ct, ROW(NULL, 'ss')::r5ct]), (12, NULL, '9', NULL, '-infinity', '[0,10)', '{[1,3),[5,7)}', 'running runners ran; the runner runs', NULL);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -1 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 20 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO ALSO INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (-1, 'a', 'infinity'), (6, 'b', '2025-01-01'), (13, 'c', '2025-01-01'), (11, 'd', NULL), (3, 'a', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (29, 'x0', NULL), (28, 'x1', NULL), (32, 'x2', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'zoë1', 1, '{a,b}', '{"seats": 1.50}'), (2, 'u2@x.org', 'Éva2', 2, '{a}', NULL), (3, 'u3@x.org', 'bob 3', 3, '{}', '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 9.99, 'void', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 9.99, 'paid', '2025-02-22', NULL), (3, 100, 'paid', '2025-02-05', '{"gift": true}'), (1, 9.99, 'new', '2025-02-26', NULL), (1, NULL, 'void', '2025-02-05', '{"gift": true}'), (2, 12345678.90, 'void', '2025-02-01', NULL), (3, 0, 'new', '2025-02-20', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, 91, '', -12.125, 2.25, '2025-11-02 05:30:00+00'), (2, 8, 190, 'note 1', 3.333, 'Infinity', '2025-03-09 07:00:00+00'), (3, 1, 92, 'NOTE 3', -12.125, -0, '2024-02-29 12:00:00+00'), (4, 4, NULL, 'note 1', 0, 'Infinity', '2025-11-02 05:30:00+00'), (5, 7, 122, 'n%5', 0.5, 'Infinity', NULL), (6, 8, 64, 'Note 3', NULL, NULL, '2024-02-29 12:00:00+00'), (7, 1, 100, 'Note 3', -12.125, NULL, '2025-03-09 06:59:59+00'), (8, 1, 68, 'note 2', 0.5, -0, '2025-11-02 05:30:00+00'), (9, 1, -14, 'NOTE 3', 3.333, NULL, '2025-11-02 05:30:00+00'), (10, 4, 50, 'note 1', 0, 'NaN', '2025-03-09 07:00:00+00'), (11, 1, 3, 'note 2', 0, 0.5, NULL), (12, 4, 14, 'NOTE 3', 0, NULL, '2025-03-09 06:59:59+00'), (13, 1, 161, 'é', 3.333, 0.5, '2025-11-02 05:30:00+00'), (14, 1, NULL, 'n%5', -12.125, 2.25, '2025-11-02 05:30:00+00'), (15, 1, 142, 'note 1', 0, 0.5, '2025-03-09 07:00:00+00'), (16, 1, 29, 'NOTE 3', -12.125, 1e300, '2025-03-09 06:59:59+00'), (17, 1, 176, 'NOTE 3', 0.5, NULL, '2025-03-09 06:59:59+00'), (18, 1, 54, 'Note 3', 0.5, 2.25, '2025-03-09 06:59:59+00'), (19, 1, -50, 'note_4', NULL, -0, '2024-02-29 12:00:00+00');

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

INSERT INTO r7t VALUES (1, 'é', 'é'), (2, 'ä', 'ä'), (3, 'ss', 'ss'), (4, '9', '9'), (5, 'Æ', 'Æ'), (6, 'ss', 'ss'), (7, 'ä', 'ä'), (8, 'a', 'a'), (9, 'ae', 'ae'), (10, 'e', 'e'), (11, 'b', 'b');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 2, NULL, 10, 'B', '2025-01-01'::timestamp + interval '20 hours'), (2, 2, 7, 1.50, NULL, '2025-01-01'::timestamp + interval '34 hours'), (3, 4, 6, 1.50, 'B', '2025-01-01'::timestamp + interval '1 hours'), (4, 2, -1, 1.50, 'ab', '2025-01-01'::timestamp + interval '114 hours'), (5, 2, 3, 0, '', '2025-01-01'::timestamp + interval '20 hours'), (6, 3, 2, 10, 'ab', '2025-01-01'::timestamp + interval '210 hours'), (7, 3, 5, 10, 'z', '2025-01-01'::timestamp + interval '99 hours'), (8, 2, 19, 10, 'ab', '2025-01-01'::timestamp + interval '24 hours'), (9, 2, 19, 10, 'ab', '2025-01-01'::timestamp + interval '49 hours'), (10, 1, 11, -2.25, NULL, '2025-01-01'::timestamp + interval '120 hours'), (11, 3, 10, 0, 'b', '2025-01-01'::timestamp + interval '96 hours'), (12, 3, 13, 1.50, 'B', '2025-01-01'::timestamp + interval '45 hours'), (13, 4, 4, -2.25, 'ab', '2025-01-01'::timestamp + interval '42 hours'), (14, 3, 20, 10, 'a', '2025-01-01'::timestamp + interval '210 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 3, 4), (2, 5, 2), (3, 5, 7), (6, 3, 4), (6, 1, 4), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{(,-2)}', '[2025-01-01,2025-02-01)', '[0,0]'), (2, '[3,3)', '{}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (3, '(-1,-1]', NULL, '[2025-03-01,infinity)', NULL), (4, '[10,)', NULL, '(2024-12-31,2025-01-05]', '(,0]'), (5, '[2,6)', '{[2,6), [8, 10)}', NULL, '[1.5,2.5)'), (6, 'empty', '{(,-3)}', 'empty', '[1.5,2.5)'), (7, 'empty', NULL, '[2025-03-01,infinity)', '[0,0]'), (8, '(9,12]', '{}', '[2025-03-01,infinity)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'a rat ran'), (2, 'jumps over the lazy dog'), (3, 'über café naïve'), (4, 'brown''s fox''s den');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '[1, "two", 3.5, null, true, {"k": [10, 20]}]');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3597) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1661) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2523) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

SET work_mem = '64kB';

RESET work_mem;

CREATE FUNCTION r9f64848(int, int) RETURNS bigint LANGUAGE sql STABLE AS $f$SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2$f$;

DROP FUNCTION r9f64848;

CREATE TABLE r8c33374 (id int NOT NULL, a text, b int DEFAULT 42, c date);

COPY r8c33374 (id, b, a, c) FROM STDIN (FORMAT csv, HEADER MATCH) WHERE b IS DISTINCT FROM 1;
id,a,b,c
1,\D,zz
2,"q""q",zz,
3,\D,,2025-02-29
4,multi word,3,,extra
\.

COPY r8c33374 TO STDOUT (FORMAT csv, HEADER, FORCE_QUOTE *);

COPY r8c33374 FROM STDIN (FORMAT csv, FORCE_QUOTE (a));

DROP TABLE r8c33374;

CREATE TABLE r9i59895 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i59895_1 PARTITION OF r9i59895 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i59895_2 PARTITION OF r9i59895 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i59895 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3473) i;

BEGIN;

CREATE INDEX r9i59895_x0 ON r9i59895 USING btree (a) INCLUDE (t);

CREATE INDEX r9i59895_x1 ON r9i59895 USING brin (id);

ALTER TABLE r9i59895 ADD CONSTRAINT r9i59895_ex EXCLUDE USING gist (p WITH ~=);

ROLLBACK;

INSERT INTO r9i59895 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(3474, 3517) i;

UPDATE r9i59895 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i59895 WHERE id % 11 = 0;

SET enable_seqscan = on;

SET enable_bitmapscan = off;

EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i59895 ORDER BY p <-> point '(3,3)', id LIMIT 5;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i59895 WHERE p <@ box '((0,0),(5,5))';

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i59895;

CREATE FUNCTION r9f39465(int, int) RETURNS bigint LANGUAGE plpgsql STABLE AS $f$BEGIN RETURN (SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2); END$f$;

DROP FUNCTION r9f39465;

CREATE TABLE r8t94962 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t94962 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf94962() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t94962', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf94962_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t94962', r);
  RETURN NULL;
END $$;

CREATE TRIGGER j_bad AFTER INSERT ON r8t94962 REFERENCING OLD TABLE AS ot FOR EACH ROW EXECUTE FUNCTION r8tf94962_tt();

CREATE TRIGGER e_tr BEFORE TRUNCATE ON r8t94962 FOR EACH STATEMENT EXECUTE FUNCTION r8tf94962();

INSERT INTO r8t94962 VALUES (10, 2, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't94962';

UPDATE r8t94962 SET v = v WHERE id = 1 RETURNING id, v, s;

DELETE FROM r8log WHERE tg = 't94962';

INSERT INTO r8t94962 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't94962';

DROP TABLE r8t94962;

DROP FUNCTION r8tf94962, r8tf94962_tt;

\s2 BEGIN;

\s2 UPDATE r7p SET region = CASE region WHEN 'eu' THEN 'us' ELSE 'eu' END WHERE id = 1;

\s3& DELETE FROM r7p WHERE id = 1 RETURNING id;

\s2 COMMIT;

\s3.;

\s2 ROLLBACK;

\s3 ROLLBACK;

CREATE TABLE r9a57301 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a57301 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4445, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4445) i;

ALTER TABLE r9a57301 ALTER COLUMN f SET STATISTICS 100;

CREATE STATISTICS r9a57301_st (mcv) ON (g + 1), b FROM r9a57301;

UPDATE r9a57301 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a57301;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a57301 WHERE u = 5$$);

SELECT * FROM r9est($$SELECT * FROM r9a57301 WHERE f < 10$$);

SELECT * FROM r9est($$SELECT * FROM r9a57301 WHERE s = 'v7'$$);

DROP TABLE r9a57301;

CREATE FUNCTION r9f17683(int, int) RETURNS bigint LANGUAGE sql STABLE AS $f$SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2$f$;

DROP FUNCTION r9f17683;

EXPLAIN (COSTS OFF) SELECT count(*) FILTER (WHERE a IS NULL), count(*) FILTER (WHERE NOT (a > 0)), count(*) FILTER (WHERE a IN (1, NULL)), bool_and(a > -10001), bool_or(f = 'NaN'), count(DISTINCT f), max(f), min(f), sum(f) FILTER (WHERE f > -9) FROM r9n WHERE id > 210;

CREATE TABLE r9a28164 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a28164 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4092, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4092) i;

CREATE STATISTICS r9a28164_st (mcv) ON g, u, b FROM r9a28164;

VACUUM ANALYZE r9a28164;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a28164 WHERE f < 10$$);

SELECT * FROM r9est($$SELECT * FROM r9a28164 WHERE u = 5$$);

DROP TABLE r9a28164;

SELECT JSON_EXISTS(jsonb '{"a": "2", "b": [], "s": "2025-03-09T02:30:00", "t": "12:30:45.5+05:30"}', '$.items[*] ? (exists(@.tags[*] ? (@ == "q"))).id' PASSING 1 AS x TRUE ON ERROR), JSON_EXISTS(jsonb '{"a": "2", "b": [], "s": "2025-03-09T02:30:00", "t": "12:30:45.5+05:30"}', 'strict $.items[*] ? (exists(@.tags[*] ? (@ == "q"))).id' TRUE ON ERROR);

CREATE TABLE r9c71252 (id int PRIMARY KEY, a int, b text, s serial) WITH (autovacuum_enabled = off);

CREATE TABLE r9c71252log (n serial, op text, id int, b text);

CREATE FUNCTION r9c71252fb() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN IF NEW.a < -3 THEN RETURN NULL; END IF; IF NEW.a = 13 THEN RAISE EXCEPTION 'r9 bad %', NEW.id; END IF; NEW.b := upper(NEW.b); RETURN NEW; END $f$;

CREATE FUNCTION r9c71252fa() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c71252log (op, id, b) VALUES (TG_WHEN || ' ' || TG_LEVEL, NEW.id, NEW.b); RETURN NULL; END $f$;

CREATE FUNCTION r9c71252fs() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c71252log (op, id, b) SELECT 'stmt', count(*), max(b) FROM nt; RETURN NULL; END $f$;

CREATE TRIGGER r9c71252a AFTER INSERT ON r9c71252 FOR EACH ROW EXECUTE FUNCTION r9c71252fa();

BEGIN;

COPY r9c71252 (id, a, b) FROM STDIN (FORMAT csv);
1,2,b1
2,9,b2
3,16,b3
4,23,b4
5,30,b5
6,37,b6
7,44,b7
8,1,b8
9,8,b9
10,15,b10
11,22,b11
12,29,b12
13,36,b13
14,43,b14
15,0,b15
16,7,b16
17,14,
18,21,b18
19,28,b19
20,35,b20
21,42,b21
22,-1,b22
23,6,b23
24,13,b24
25,20,b25
26,27,b26
27,34,b27
28,41,b28
29,-2,b29
30,5,b30
31,12,b31
32,19,b32
33,26,b33
34,33,
35,40,b35
36,-3,b36
37,4,b37
38,11,b38
39,18,b39
40,25,b40
41,32,b41
42,39,b42
43,-4,b43
44,3,b44
45,10,b45
46,17,b46
47,24,b47
48,31,b48
49,38,b49
50,-5,b50
51,2,
52,9,b52
53,16,b53
54,23,b54
55,30,b55
56,37,b56
57,44,b57
58,1,b58
59,8,b59
60,15,b60
61,22,b61
62,29,b62
63,36,b63
64,43,b64
65,0,b65
66,7,b66
67,14,b67
68,21,
69,28,b69
70,35,b70
71,42,b71
72,-1,b72
73,6,b73
74,13,b74
75,20,b75
76,27,b76
77,34,b77
78,41,b78
79,-2,b79
80,5,b80
81,12,b81
82,19,b82
83,26,b83
84,33,b84
85,40,
86,-3,b86
87,4,b87
88,11,b88
89,18,b89
90,25,b90
91,32,b91
92,39,b92
93,-4,b93
94,3,b94
95,10,b95
96,17,b96
97,24,b97
98,31,b98
99,38,b99
100,-5,b100
101,2,b101
102,9,
103,16,b103
104,23,b104
105,30,b105
106,37,b106
107,44,b107
108,1,b108
109,8,b109
110,15,b110
111,22,b111
112,29,b112
113,36,b113
114,43,b114
115,0,b115
116,7,b116
117,14,b117
118,21,b118
119,28,
120,35,b120
121,42,b121
122,-1,b122
123,6,b123
124,13,b124
125,20,b125
126,27,b126
127,34,b127
128,41,b128
129,-2,b129
130,5,b130
131,12,b131
132,19,b132
133,26,b133
134,33,b134
135,40,b135
136,-3,
137,4,b137
138,11,b138
139,18,b139
140,25,b140
141,32,b141
142,39,b142
143,-4,b143
144,3,b144
145,10,b145
146,17,b146
147,24,b147
148,31,b148
149,38,b149
150,-5,b150
151,2,b151
152,9,b152
153,16,
154,23,b154
155,30,b155
156,37,b156
157,44,b157
158,1,b158
159,8,b159
160,15,b160
161,22,b161
162,29,b162
163,36,b163
164,43,b164
165,0,b165
166,7,b166
167,14,b167
168,21,b168
169,28,b169
170,35,
171,42,b171
172,-1,b172
173,6,b173
174,13,b174
175,20,b175
176,27,b176
177,34,b177
178,41,b178
179,-2,b179
180,5,b180
181,12,b181
182,19,b182
183,26,b183
184,33,b184
185,40,b185
186,-3,b186
187,4,
188,11,b188
189,18,b189
190,25,b190
191,32,b191
192,39,b192
193,-4,b193
194,3,b194
195,10,b195
196,17,b196
197,24,b197
198,31,b198
199,38,b199
200,-5,b200
201,2,b201
202,9,b202
203,16,b203
204,23,
205,30,b205
206,37,b206
207,44,b207
208,1,b208
209,8,b209
210,15,b210
211,22,b211
212,29,b212
213,36,b213
214,43,b214
215,0,b215
216,7,b216
217,14,b217
218,21,b218
219,28,b219
220,35,b220
221,42,
222,-1,b222
223,6,b223
224,13,b224
225,20,b225
226,27,b226
227,34,b227
228,41,b228
229,-2,b229
230,5,b230
231,12,b231
232,19,b232
233,26,b233
234,33,b234
235,40,b235
236,-3,b236
237,4,b237
238,11,
239,18,b239
240,25,b240
241,32,b241
242,39,b242
243,-4,b243
244,3,b244
245,10,b245
246,17,b246
247,24,b247
248,31,b248
249,38,b249
250,-5,b250
251,2,b251
252,9,b252
253,16,b253
254,23,b254
255,30,
256,37,b256
257,44,b257
258,1,b258
259,8,b259
260,15,b260
261,22,b261
262,29,b262
263,36,b263
264,43,b264
265,0,b265
266,7,b266
267,14,b267
268,21,b268
269,28,b269
270,35,b270
271,42,b271
272,-1,
273,6,b273
274,13,b274
275,20,b275
276,27,b276
277,34,b277
278,41,b278
279,-2,b279
280,5,b280
281,12,b281
282,19,b282
283,26,b283
284,33,b284
285,40,b285
286,-3,b286
287,4,b287
288,11,b288
289,18,
290,25,b290
291,32,b291
292,39,b292
293,-4,b293
294,3,b294
295,10,b295
296,17,b296
297,24,b297
298,31,b298
299,38,b299
300,-5,b300
301,2,b301
302,9,b302
303,16,b303
304,23,b304
305,30,b305
306,37,
307,44,b307
308,1,b308
309,8,b309
310,15,b310
311,22,b311
312,29,b312
313,36,b313
314,43,b314
315,0,b315
316,7,b316
317,14,b317
318,21,b318
319,28,b319
320,35,b320
321,42,b321
322,-1,b322
323,6,
324,13,b324
325,20,b325
326,27,b326
327,34,b327
328,41,b328
329,-2,b329
330,5,b330
331,12,b331
332,19,b332
333,26,b333
334,33,b334
335,40,b335
336,-3,b336
337,4,b337
338,11,b338
339,18,b339
340,25,
341,32,b341
342,39,b342
343,-4,b343
344,3,b344
345,10,b345
346,17,b346
347,24,b347
348,31,b348
349,38,b349
350,-5,b350
351,2,b351
352,9,b352
353,16,b353
354,23,b354
355,30,b355
356,37,b356
357,44,
358,1,b358
359,8,b359
360,15,b360
361,22,b361
362,29,b362
363,36,b363
364,43,b364
365,0,b365
366,7,b366
367,14,b367
368,21,b368
369,28,b369
370,35,b370
371,42,b371
372,-1,b372
373,6,b373
374,13,
375,20,b375
376,27,b376
377,34,b377
378,41,b378
379,-2,b379
380,5,b380
381,12,b381
382,19,b382
383,26,b383
384,33,b384
385,40,b385
386,-3,b386
387,4,b387
388,11,b388
389,18,b389
390,25,b390
391,32,
392,39,b392
393,-4,b393
394,3,b394
395,10,b395
396,17,b396
397,24,b397
398,31,b398
399,38,b399
400,-5,b400
401,2,b401
402,9,b402
403,16,b403
404,23,b404
405,30,b405
406,37,b406
407,44,b407
408,1,
409,8,b409
410,15,b410
411,22,b411
412,29,b412
413,36,b413
414,43,b414
415,0,b415
416,7,b416
417,14,b417
418,21,b418
419,28,b419
420,35,b420
421,42,b421
422,-1,b422
423,6,b423
424,13,b424
425,20,
426,27,b426
427,34,b427
428,41,b428
429,-2,b429
430,5,b430
431,12,b431
432,19,b432
433,26,b433
434,33,b434
435,40,b435
436,-3,b436
437,4,b437
438,11,b438
439,18,b439
440,25,b440
441,32,b441
442,39,
443,-4,b443
444,3,b444
445,10,b445
446,17,b446
447,24,b447
448,31,b448
449,38,b449
450,-5,b450
451,2,b451
452,9,b452
453,16,b453
454,23,b454
455,30,b455
456,37,b456
457,44,b457
458,1,b458
459,8,
460,15,b460
461,22,b461
462,29,b462
463,36,b463
464,43,b464
465,0,b465
466,7,b466
467,14,b467
468,21,b468
469,28,b469
470,35,b470
471,42,b471
472,-1,b472
473,6,b473
474,13,b474
475,20,b475
476,27,
477,34,b477
478,41,b478
479,-2,b479
480,5,b480
481,12,b481
482,19,b482
483,26,b483
484,33,b484
485,40,b485
486,-3,b486
487,4,b487
488,11,b488
489,18,b489
490,25,b490
491,32,b491
492,39,b492
493,-4,
494,3,b494
495,10,b495
496,17,b496
497,24,b497
498,31,b498
499,38,b499
500,-5,b500
501,2,b501
502,9,b502
503,16,b503
504,23,b504
505,30,b505
506,37,b506
507,44,b507
508,1,b508
509,8,b509
510,15,
511,22,b511
512,29,b512
513,36,b513
514,43,b514
515,0,b515
516,7,b516
517,14,b517
518,21,b518
519,28,b519
520,35,b520
521,42,b521
522,-1,b522
523,6,b523
524,13,b524
525,20,b525
526,27,b526
527,34,
528,41,b528
529,-2,b529
530,5,b530
531,12,b531
532,19,b532
533,26,b533
534,33,b534
535,40,b535
536,-3,b536
537,4,b537
538,11,b538
539,18,b539
540,25,b540
541,32,b541
542,39,b542
543,-4,b543
544,3,
545,10,b545
546,17,b546
547,24,b547
548,31,b548
549,38,b549
550,-5,b550
551,2,b551
552,9,b552
553,16,b553
554,23,b554
555,30,b555
556,37,b556
557,44,b557
558,1,b558
559,8,b559
560,15,b560
561,22,
562,29,b562
563,36,b563
564,43,b564
565,0,b565
566,7,b566
567,14,b567
568,21,b568
569,28,b569
570,35,b570
571,42,b571
572,-1,b572
573,6,b573
574,13,b574
575,20,b575
576,27,b576
577,34,b577
578,41,
579,-2,b579
580,5,b580
581,12,b581
582,19,b582
583,26,b583
584,33,b584
585,40,b585
586,-3,b586
587,4,b587
588,11,b588
589,18,b589
590,25,b590
591,32,b591
592,39,b592
593,-4,b593
594,3,b594
595,10,
596,17,b596
597,24,b597
598,31,b598
599,38,b599
600,-5,b600
601,2,b601
602,9,b602
603,16,b603
604,23,b604
605,30,b605
606,37,b606
607,44,b607
608,1,b608
609,8,b609
610,15,b610
611,22,b611
612,29,
613,36,b613
614,43,b614
615,0,b615
616,7,b616
617,14,b617
618,21,b618
619,28,b619
620,35,b620
621,42,b621
622,-1,b622
623,6,b623
624,13,b624
625,20,b625
626,27,b626
627,34,b627
628,41,b628
629,-2,
630,5,b630
631,12,b631
632,19,b632
633,26,b633
634,33,b634
635,40,b635
636,-3,b636
637,4,b637
638,11,b638
639,18,b639
640,25,b640
641,32,b641
642,39,b642
643,-4,b643
644,3,b644
645,10,b645
646,17,
647,24,b647
648,31,b648
649,38,b649
650,-5,b650
651,2,b651
652,9,b652
653,16,b653
654,23,b654
655,30,b655
656,37,b656
657,44,b657
658,1,b658
659,8,b659
660,15,b660
661,22,b661
662,29,b662
663,36,
664,43,b664
665,0,b665
666,7,b666
667,14,b667
668,21,b668
669,28,b669
670,35,b670
671,42,b671
672,-1,b672
673,6,b673
674,13,b674
675,20,b675
676,27,b676
677,34,b677
678,41,b678
679,-2,b679
680,5,
681,12,b681
682,19,b682
683,26,b683
684,33,b684
685,40,b685
686,-3,b686
687,4,b687
688,11,b688
689,18,b689
690,25,b690
691,32,b691
692,39,b692
693,-4,b693
694,3,b694
695,10,b695
696,17,b696
697,24,
698,31,b698
699,38,b699
700,-5,b700
701,2,b701
702,9,b702
703,16,b703
704,23,b704
705,30,b705
706,37,b706
707,44,b707
708,1,b708
709,8,b709
710,15,b710
711,22,b711
712,29,b712
713,36,b713
714,43,
715,0,b715
716,7,b716
717,14,b717
718,21,b718
719,28,b719
720,35,b720
721,42,b721
722,-1,b722
723,6,b723
724,13,b724
725,20,b725
726,27,b726
727,34,b727
728,41,b728
729,-2,b729
730,5,b730
731,12,
732,19,b732
733,26,b733
734,33,b734
735,40,b735
736,-3,b736
737,4,b737
738,11,b738
739,18,b739
740,25,b740
741,32,b741
742,39,b742
743,-4,b743
744,3,b744
745,10,b745
746,17,b746
747,24,b747
748,31,
749,38,b749
750,-5,b750
751,2,b751
752,9,b752
753,16,b753
754,23,b754
755,30,b755
756,37,b756
757,44,b757
758,1,b758
759,8,b759
760,15,b760
761,22,b761
762,29,b762
763,36,b763
764,43,b764
765,0,
766,7,b766
767,14,b767
768,21,b768
769,28,b769
770,35,b770
771,42,b771
772,-1,b772
773,6,b773
774,13,b774
775,20,b775
776,27,b776
777,34,b777
778,41,b778
779,-2,b779
780,5,b780
781,12,b781
782,19,
783,26,b783
784,33,b784
785,40,b785
786,-3,b786
787,4,b787
788,11,b788
789,18,b789
790,25,b790
791,32,b791
792,39,b792
793,-4,b793
794,3,b794
795,10,b795
796,17,b796
797,24,b797
798,31,b798
799,38,
800,-5,b800
801,2,b801
802,9,b802
803,16,b803
804,23,b804
805,30,b805
806,37,b806
807,44,b807
808,1,b808
809,8,b809
810,15,b810
811,22,b811
812,29,b812
813,36,b813
814,43,b814
815,0,b815
816,7,
817,14,b817
818,21,b818
819,28,b819
820,35,b820
821,42,b821
822,-1,b822
823,6,b823
824,13,b824
825,20,b825
826,27,b826
827,34,b827
828,41,b828
829,-2,b829
830,5,b830
831,12,b831
832,19,b832
833,26,
834,33,b834
835,40,b835
836,-3,b836
837,4,b837
838,11,b838
839,18,b839
840,25,b840
841,32,b841
842,39,b842
843,-4,b843
844,3,b844
845,10,b845
846,17,b846
847,24,b847
848,31,b848
849,38,b849
850,-5,
851,2,b851
852,9,b852
853,16,b853
854,23,b854
855,30,b855
856,37,b856
857,44,b857
858,1,b858
859,8,b859
860,15,b860
861,22,b861
862,29,b862
863,36,b863
864,43,b864
865,0,b865
866,7,b866
867,14,
868,21,b868
869,28,b869
870,35,b870
871,42,b871
872,-1,b872
873,6,b873
874,13,b874
875,20,b875
876,27,b876
877,34,b877
878,41,b878
879,-2,b879
880,5,b880
881,12,b881
882,19,b882
883,26,b883
884,33,
885,40,b885
886,-3,b886
887,4,b887
888,11,b888
889,18,b889
890,25,b890
891,32,b891
892,39,b892
893,-4,b893
894,3,b894
895,10,b895
896,17,b896
897,24,b897
898,31,b898
899,38,b899
900,-5,b900
901,2,
902,9,b902
903,16,b903
904,23,b904
905,30,b905
906,37,b906
907,44,b907
908,1,b908
909,8,b909
910,15,b910
911,22,b911
912,29,b912
913,36,b913
914,43,b914
915,0,b915
916,7,b916
917,14,b917
918,21,
919,28,b919
920,35,b920
921,42,b921
922,-1,b922
923,6,b923
924,13,b924
925,20,b925
926,27,b926
927,34,b927
928,41,b928
929,-2,b929
930,5,b930
931,12,b931
932,19,b932
933,26,b933
934,33,b934
935,40,
936,-3,b936
937,4,b937
938,11,b938
939,18,b939
940,25,b940
941,32,b941
942,39,b942
943,-4,b943
944,3,b944
945,10,b945
946,17,b946
947,24,b947
948,31,b948
949,38,b949
950,-5,b950
951,2,b951
952,9,
953,16,b953
954,23,b954
955,30,b955
956,37,b956
957,44,b957
958,1,b958
959,8,b959
960,15,b960
961,22,b961
962,29,b962
963,36,b963
964,43,b964
965,0,b965
966,7,b966
967,14,b967
968,21,b968
969,28,
970,35,b970
971,42,b971
972,-1,b972
973,6,b973
974,13,b974
975,20,b975
976,27,b976
977,34,b977
978,41,b978
979,-2,b979
980,5,b980
981,12,b981
982,19,b982
983,26,b983
984,33,b984
985,40,b985
986,-3,
987,4,b987
988,11,b988
989,18,b989
990,25,b990
991,32,b991
992,39,b992
993,-4,b993
994,3,b994
995,10,b995
996,17,b996
997,24,b997
998,31,b998
999,38,b999
1000,-5,b1000
1001,2,b1001
1002,9,b1002
1003,16,
1004,23,b1004
1005,30,b1005
1006,37,b1006
1007,44,b1007
1008,1,b1008
1009,8,b1009
1010,15,b1010
1011,22,b1011
1012,29,b1012
1013,36,b1013
1014,43,b1014
1015,0,b1015
1016,7,b1016
1017,14,b1017
1018,21,b1018
1019,28,b1019
1020,35,
1021,42,b1021
1022,-1,b1022
1023,6,b1023
1024,13,b1024
1025,20,b1025
1026,27,b1026
1027,34,b1027
1028,41,b1028
1029,-2,b1029
1030,5,b1030
1031,12,b1031
1032,19,b1032
1033,26,b1033
1034,33,b1034
1035,40,b1035
1036,-3,b1036
1037,4,
1038,11,b1038
1039,18,b1039
1040,25,b1040
1041,32,b1041
1042,39,b1042
1043,-4,b1043
1044,3,b1044
1045,10,b1045
1046,17,b1046
1047,24,b1047
1048,31,b1048
1049,38,b1049
1050,-5,b1050
1051,2,b1051
1052,9,b1052
1053,16,b1053
1054,23,
1055,30,b1055
1056,37,b1056
1057,44,b1057
1058,1,b1058
1059,8,b1059
1060,15,b1060
1061,22,b1061
1062,29,b1062
1063,36,b1063
1064,43,b1064
1065,0,b1065
1066,7,b1066
1067,14,b1067
1068,21,b1068
1069,28,b1069
1070,35,b1070
1071,42,
1072,-1,b1072
1073,6,b1073
1074,13,b1074
1075,20,b1075
1076,27,b1076
1077,34,b1077
1078,41,b1078
1079,-2,b1079
1080,5,b1080
1081,12,b1081
1082,19,b1082
1083,26,b1083
1084,33,b1084
1085,40,b1085
1086,-3,b1086
1087,4,b1087
1088,11,
1089,18,b1089
1090,25,b1090
1091,32,b1091
1092,39,b1092
1093,-4,b1093
1094,3,b1094
1095,10,b1095
1096,17,b1096
1097,24,b1097
1098,31,b1098
1099,38,b1099
1100,-5,b1100
1101,2,b1101
1102,9,b1102
1103,16,b1103
1104,23,b1104
1105,30,
1106,37,b1106
1107,44,b1107
1108,1,b1108
1109,8,b1109
1110,15,b1110
1111,22,b1111
1112,29,b1112
1113,36,b1113
1114,43,b1114
1115,0,b1115
1116,7,b1116
1117,14,b1117
1118,21,b1118
1119,28,b1119
1120,35,b1120
1121,42,b1121
1122,-1,
1123,6,b1123
1124,13,b1124
1125,20,b1125
1126,27,b1126
1127,34,b1127
1128,41,b1128
1129,-2,b1129
1130,5,b1130
1131,12,b1131
1132,19,b1132
1133,26,b1133
1134,33,b1134
1135,40,b1135
1136,-3,b1136
1137,4,b1137
1138,11,b1138
1139,18,
1140,25,b1140
1141,32,b1141
1142,39,b1142
1143,-4,b1143
1144,3,b1144
1145,10,b1145
1146,17,b1146
1147,24,b1147
1148,31,b1148
1149,38,b1149
1150,-5,b1150
1151,2,b1151
1152,9,b1152
1153,16,b1153
1154,23,b1154
1155,30,b1155
1156,37,
1157,44,b1157
1158,1,b1158
1159,8,b1159
1160,15,b1160
1161,22,b1161
1162,29,b1162
1163,36,b1163
1164,43,b1164
1165,0,b1165
1166,7,b1166
1167,14,b1167
1168,21,b1168
1169,28,b1169
1170,35,b1170
1171,42,b1171
1172,-1,b1172
1173,6,
1174,13,b1174
1175,20,b1175
1176,27,b1176
1177,34,b1177
1178,41,b1178
1179,-2,b1179
1180,5,b1180
1181,12,b1181
1182,19,b1182
1183,26,b1183
1184,33,b1184
1185,40,b1185
1186,-3,b1186
1187,4,b1187
1188,11,b1188
1189,18,b1189
1190,25,
1191,32,b1191
1192,39,b1192
1193,-4,b1193
1194,3,b1194
1195,10,b1195
1196,17,b1196
1197,24,b1197
1198,31,b1198
1199,38,b1199
1200,-5,b1200
1201,2,b1201
1202,9,b1202
1203,16,b1203
1204,23,b1204
1205,30,b1205
1206,37,b1206
1207,44,
1208,1,b1208
1209,8,b1209
1210,15,b1210
1211,22,b1211
1212,29,b1212
1213,36,b1213
1214,43,b1214
1215,0,b1215
1216,7,b1216
1217,14,b1217
1218,21,b1218
1219,28,b1219
1220,35,b1220
1221,42,b1221
1222,-1,b1222
1223,6,b1223
1224,13,
1225,20,b1225
1226,27,b1226
1227,34,b1227
1228,41,b1228
1229,-2,b1229
1230,5,b1230
1231,12,b1231
1232,19,b1232
1233,26,b1233
1234,33,b1234
1235,40,b1235
1236,-3,b1236
1237,4,b1237
1238,11,b1238
1239,18,b1239
1240,25,b1240
1241,32,
1242,39,b1242
1243,-4,b1243
1244,3,b1244
1245,10,b1245
1246,17,b1246
1247,24,b1247
1248,31,b1248
1249,38,b1249
1250,-5,b1250
1251,2,b1251
1252,9,b1252
1253,16,b1253
1254,23,b1254
1255,30,b1255
1256,37,b1256
1257,44,b1257
1258,1,
1259,8,b1259
1260,15,b1260
1261,22,b1261
1262,29,b1262
1263,36,b1263
1264,43,b1264
1265,0,b1265
1266,7,b1266
1267,14,b1267
1268,21,b1268
1269,28,b1269
1270,35,b1270
1271,42,b1271
1272,-1,b1272
1273,6,b1273
1274,13,b1274
1275,20,
1276,27,b1276
1277,34,b1277
1278,41,b1278
1279,-2,b1279
1280,5,b1280
1281,12,b1281
1282,19,b1282
1283,26,b1283
1284,33,b1284
1285,40,b1285
1286,-3,b1286
1287,4,b1287
1288,11,b1288
1289,18,b1289
1290,25,b1290
1291,32,b1291
1292,39,
1293,-4,b1293
1294,3,b1294
1295,10,b1295
1296,17,b1296
1297,24,b1297
1298,31,b1298
1299,38,b1299
1300,-5,b1300
1301,2,b1301
1302,9,b1302
1303,16,b1303
1304,23,b1304
1305,30,b1305
1306,37,b1306
1307,44,b1307
1308,1,b1308
1309,8,
1310,15,b1310
1311,22,b1311
1312,29,b1312
1313,36,b1313
1314,43,b1314
1315,0,b1315
1316,7,b1316
1317,14,b1317
1318,21,b1318
1319,28,b1319
1320,35,b1320
1321,42,b1321
1322,-1,b1322
1323,6,b1323
1324,13,b1324
1325,20,b1325
1326,27,
1327,34,b1327
1328,41,b1328
1329,-2,b1329
1330,5,b1330
1331,12,b1331
1332,19,b1332
1333,26,b1333
1334,33,b1334
1335,40,b1335
1336,-3,b1336
1337,4,b1337
1338,11,b1338
1339,18,b1339
1340,25,b1340
1341,32,b1341
1342,39,b1342
1343,-4,
1344,3,b1344
1345,10,b1345
1346,17,b1346
1347,24,b1347
1348,31,b1348
1349,38,b1349
1350,-5,b1350
1351,2,b1351
1352,9,b1352
1353,16,b1353
1354,23,b1354
1355,30,b1355
1356,37,b1356
1357,44,b1357
1358,1,b1358
1359,8,b1359
1360,15,
1361,22,b1361
1362,29,b1362
1363,36,b1363
1364,43,b1364
1365,0,b1365
1366,7,b1366
1367,14,b1367
1368,21,b1368
1369,28,b1369
1370,35,b1370
1371,42,b1371
1372,-1,b1372
1373,6,b1373
1374,13,b1374
1375,20,b1375
1376,27,b1376
1377,34,
1378,41,b1378
1379,-2,b1379
1380,5,b1380
1381,12,b1381
1382,19,b1382
1383,26,b1383
1384,33,b1384
1385,40,b1385
1386,-3,b1386
1387,4,b1387
1388,11,b1388
1389,18,b1389
1390,25,b1390
1391,32,b1391
1392,39,b1392
1393,-4,b1393
1394,3,
1395,10,b1395
1396,17,b1396
1397,24,b1397
1398,31,b1398
1399,38,b1399
1400,-5,b1400
1401,2,b1401
1402,9,b1402
1403,16,b1403
1404,23,b1404
1405,30,b1405
1406,37,b1406
1407,44,b1407
1408,1,b1408
1409,8,b1409
1410,15,b1410
1411,22,
1412,29,b1412
1413,36,b1413
1414,43,b1414
1415,0,b1415
1416,7,b1416
1417,14,b1417
1418,21,b1418
1419,28,b1419
1420,35,b1420
1421,42,b1421
1422,-1,b1422
1423,6,b1423
1424,13,b1424
1425,20,b1425
1426,27,b1426
1427,34,b1427
1428,41,
1429,-2,b1429
1430,5,b1430
1431,12,b1431
1432,19,b1432
1433,26,b1433
1434,33,b1434
1435,40,b1435
1436,-3,b1436
1437,4,b1437
1438,11,b1438
1439,18,b1439
1440,25,b1440
1441,32,b1441
1442,39,b1442
1443,-4,b1443
1444,3,b1444
1445,10,
1446,17,b1446
1447,24,b1447
1448,31,b1448
1449,38,b1449
1450,-5,b1450
1451,2,b1451
1452,9,b1452
1453,16,b1453
1454,23,b1454
1455,30,b1455
1456,37,b1456
1457,44,b1457
1458,1,b1458
1459,8,b1459
1460,15,b1460
1461,22,b1461
1462,29,
1463,36,b1463
1464,43,b1464
1465,0,b1465
1466,7,b1466
1467,14,b1467
1468,21,b1468
1469,28,b1469
1470,35,b1470
1471,42,b1471
1472,-1,b1472
1473,6,b1473
1474,13,b1474
1475,20,b1475
1476,27,b1476
1477,34,b1477
1478,41,b1478
1479,-2,
1480,5,b1480
1481,12,b1481
1482,19,b1482
1483,26,b1483
1484,33,b1484
1485,40,b1485
1486,-3,b1486
1487,4,b1487
1488,11,b1488
1489,18,b1489
1490,25,b1490
1491,32,b1491
1492,39,b1492
1493,-4,b1493
1494,3,b1494
1495,10,b1495
1496,17,
1497,24,b1497
1498,31,b1498
1499,38,b1499
1500,-5,b1500
1501,2,b1501
1502,9,b1502
1503,16,b1503
1504,23,b1504
1505,30,b1505
1506,37,b1506
1507,44,b1507
1508,1,b1508
1509,8,b1509
1510,15,b1510
1511,22,b1511
1512,29,b1512
1513,36,
1514,43,b1514
1515,0,b1515
1516,7,b1516
1517,14,b1517
1518,21,b1518
1519,28,b1519
1520,35,b1520
1521,42,b1521
1522,-1,b1522
1523,6,b1523
1524,13,b1524
1525,20,b1525
1526,27,b1526
1527,34,b1527
1528,41,b1528
1529,-2,b1529
1530,5,
1531,12,b1531
1532,19,b1532
1533,26,b1533
1534,33,b1534
1535,40,b1535
1536,-3,b1536
1537,4,b1537
1538,11,b1538
1539,18,b1539
1540,25,b1540
1541,32,b1541
1542,39,b1542
1543,-4,b1543
1544,3,b1544
1545,10,b1545
1546,17,b1546
1547,24,
1548,31,b1548
1549,38,b1549
1550,-5,b1550
1551,2,b1551
1552,9,b1552
1553,16,b1553
1554,23,b1554
1555,30,b1555
1556,37,b1556
1557,44,b1557
1558,1,b1558
1559,8,b1559
1560,15,b1560
1561,22,b1561
1562,29,b1562
1563,36,b1563
1564,43,
1565,0,b1565
1566,7,b1566
1567,14,b1567
1568,21,b1568
1569,28,b1569
1570,35,b1570
1571,42,b1571
1572,-1,b1572
1573,6,b1573
1574,13,b1574
1575,20,b1575
1576,27,b1576
1577,34,b1577
1578,41,b1578
1579,-2,b1579
1580,5,b1580
1581,12,
1582,19,b1582
1583,26,b1583
1584,33,b1584
1585,40,b1585
1586,-3,b1586
1587,4,b1587
1588,11,b1588
1589,18,b1589
1590,25,b1590
1591,32,b1591
1592,39,b1592
1593,-4,b1593
1594,3,b1594
1595,10,b1595
1596,17,b1596
1597,24,b1597
1598,31,
1599,38,b1599
1600,-5,b1600
1601,2,b1601
1602,9,b1602
1603,16,b1603
1604,23,b1604
1605,30,b1605
1606,37,b1606
1607,44,b1607
1608,1,b1608
1609,8,b1609
1610,15,b1610
1611,22,b1611
1612,29,b1612
1613,36,b1613
1614,43,b1614
1615,0,
1616,7,b1616
1617,14,b1617
1618,21,b1618
1619,28,b1619
1620,35,b1620
1621,42,b1621
1622,-1,b1622
1623,6,b1623
1624,13,b1624
1625,20,b1625
1626,27,b1626
1627,34,b1627
1628,41,b1628
1629,-2,b1629
1630,5,b1630
1631,12,b1631
1632,19,
1633,26,b1633
1634,33,b1634
1635,40,b1635
1636,-3,b1636
1637,4,b1637
1638,11,b1638
1639,18,b1639
1640,25,b1640
1641,32,b1641
1642,39,b1642
1643,-4,b1643
1644,3,b1644
1645,10,b1645
1646,17,b1646
1647,24,b1647
1648,31,b1648
1649,38,
1650,-5,b1650
1651,2,b1651
1652,9,b1652
1653,16,b1653
1654,23,b1654
1655,30,b1655
1656,37,b1656
1657,44,b1657
1658,1,b1658
1659,8,b1659
1660,15,b1660
1661,22,b1661
1662,29,b1662
1663,36,b1663
1664,43,b1664
1665,0,b1665
1666,7,
1667,14,b1667
1668,21,b1668
1669,28,b1669
1670,35,b1670
1671,42,b1671
1672,-1,b1672
1673,6,b1673
1674,13,b1674
1675,20,b1675
1676,27,b1676
1677,34,b1677
1678,41,b1678
1679,-2,b1679
1680,5,b1680
1681,12,b1681
1682,19,b1682
1683,26,
1684,33,b1684
1685,40,b1685
1686,-3,b1686
1687,4,b1687
1688,11,b1688
1689,18,b1689
1690,25,b1690
1691,32,b1691
1692,39,b1692
1693,-4,b1693
1694,3,b1694
1695,10,b1695
1696,17,b1696
1697,24,b1697
1698,31,b1698
1699,38,b1699
1700,-5,
1701,2,b1701
1702,9,b1702
1703,16,b1703
1704,23,b1704
1705,30,b1705
1706,37,b1706
1707,44,b1707
1708,1,b1708
1709,8,b1709
1710,15,b1710
1711,22,b1711
1712,29,b1712
1713,36,b1713
1714,43,b1714
1715,0,b1715
1716,7,b1716
1717,14,
1718,21,b1718
1719,28,b1719
1720,35,b1720
1721,42,b1721
1722,-1,b1722
1723,6,b1723
1724,13,b1724
1725,20,b1725
1726,27,b1726
1727,34,b1727
1728,41,b1728
1729,-2,b1729
1730,5,b1730
1731,12,b1731
1732,19,b1732
1733,26,b1733
1734,33,
1735,40,b1735
1736,-3,b1736
1737,4,b1737
1738,11,b1738
1739,18,b1739
1740,25,b1740
1741,32,b1741
1742,39,b1742
1743,-4,b1743
1744,3,b1744
1745,10,b1745
1746,17,b1746
1747,24,b1747
1748,31,b1748
1749,38,b1749
1750,-5,b1750
1751,2,
1752,9,b1752
1753,16,b1753
1754,23,b1754
1755,30,b1755
1756,37,b1756
1757,44,b1757
1758,1,b1758
1759,8,b1759
1760,15,b1760
1761,22,b1761
1762,29,b1762
1763,36,b1763
1764,43,b1764
1765,0,b1765
1766,7,b1766
1767,14,b1767
1768,21,
1769,28,b1769
1770,35,b1770
1771,42,b1771
1772,-1,b1772
1773,6,b1773
1774,13,b1774
1775,20,b1775
1776,27,b1776
1777,34,b1777
1778,41,b1778
1779,-2,b1779
1780,5,b1780
1781,12,b1781
1782,19,b1782
1783,26,b1783
1784,33,b1784
1785,40,
1786,-3,b1786
1787,4,b1787
1788,11,b1788
1789,18,b1789
1790,25,b1790
1791,32,b1791
1792,39,b1792
1793,-4,b1793
1794,3,b1794
1795,10,b1795
1796,17,b1796
1797,24,b1797
1798,31,b1798
1799,38,b1799
1800,-5,b1800
1801,2,b1801
1802,9,
1803,16,b1803
1804,23,b1804
1805,30,b1805
1806,37,b1806
1807,44,b1807
1808,1,b1808
1809,8,b1809
1810,15,b1810
1811,22,b1811
1812,29,b1812
1813,36,b1813
1814,43,b1814
1815,0,b1815
1816,7,b1816
1817,14,b1817
1818,21,b1818
1819,28,
1820,35,b1820
1821,42,b1821
1822,-1,b1822
1823,6,b1823
1824,13,b1824
1825,20,b1825
1826,27,b1826
1827,34,b1827
1828,41,b1828
1829,-2,b1829
1830,5,b1830
1831,12,b1831
1832,19,b1832
1833,26,b1833
1834,33,b1834
1835,40,b1835
1836,-3,
1837,4,b1837
1838,11,b1838
1839,18,b1839
1840,25,b1840
1841,32,b1841
1842,39,b1842
1843,-4,b1843
1844,3,b1844
1845,10,b1845
1846,17,b1846
1847,24,b1847
1848,31,b1848
1849,38,b1849
1850,-5,b1850
1851,2,b1851
1852,9,b1852
1853,16,
1854,23,b1854
1855,30,b1855
1856,37,b1856
1857,44,b1857
1858,1,b1858
1859,8,b1859
1860,15,b1860
1861,22,b1861
1862,29,b1862
1863,36,b1863
1864,43,b1864
1865,0,b1865
1866,7,b1866
1867,14,b1867
1868,21,b1868
1869,28,b1869
1870,35,
1871,42,b1871
1872,-1,b1872
1873,6,b1873
1874,13,b1874
1875,20,b1875
1876,27,b1876
1877,34,b1877
1878,41,b1878
1879,-2,b1879
1880,5,b1880
1881,12,b1881
1882,19,b1882
1883,26,b1883
1884,33,b1884
1885,40,b1885
1886,-3,b1886
1887,4,
1888,11,b1888
1889,18,b1889
1890,25,b1890
1891,32,b1891
1892,39,b1892
1893,-4,b1893
1894,3,b1894
1895,10,b1895
1896,17,b1896
1897,24,b1897
1898,31,b1898
1899,38,b1899
1900,-5,b1900
1901,2,b1901
1902,9,b1902
1903,16,b1903
1904,23,
1905,30,b1905
1906,37,b1906
1907,44,b1907
1908,1,b1908
1909,8,b1909
1910,15,b1910
1911,22,b1911
1912,29,b1912
1913,36,b1913
1914,43,b1914
1915,0,b1915
1916,7,b1916
1917,14,b1917
1918,21,b1918
1919,28,b1919
1920,35,b1920
1921,42,
1922,-1,b1922
1923,6,b1923
1924,13,b1924
1925,20,b1925
1926,27,b1926
1927,34,b1927
1928,41,b1928
1929,-2,b1929
1930,5,b1930
1931,12,b1931
1932,19,b1932
1933,26,b1933
1934,33,b1934
1935,40,b1935
1936,-3,b1936
1937,4,b1937
1938,11,
1939,18,b1939
1940,25,b1940
1941,32,b1941
1942,39,b1942
1943,-4,b1943
1944,3,b1944
1945,10,b1945
1946,17,b1946
1947,24,b1947
1948,31,b1948
1949,38,b1949
1950,-5,b1950
1951,2,b1951
1952,9,b1952
1953,16,b1953
1954,23,b1954
1955,30,
1956,37,b1956
1957,44,b1957
1958,1,b1958
1959,8,b1959
1960,15,b1960
1961,22,b1961
1962,29,b1962
1963,36,b1963
1964,43,b1964
1965,0,b1965
1966,7,b1966
1967,14,b1967
1968,21,b1968
1969,28,b1969
1970,35,b1970
1971,42,b1971
1972,-1,
1973,6,b1973
1974,13,b1974
1975,20,b1975
1976,27,b1976
1977,34,b1977
1978,41,b1978
1979,-2,b1979
1980,5,b1980
1981,12,b1981
1982,19,b1982
1983,26,b1983
1984,33,b1984
1985,40,b1985
1986,-3,b1986
1987,4,b1987
1988,11,b1988
1989,18,
1990,25,b1990
1991,32,b1991
1992,39,b1992
1993,-4,b1993
1994,3,b1994
1995,10,b1995
1996,17,b1996
1997,24,b1997
1998,31,b1998
1999,38,b1999
2000,-5,b2000
2001,2,b2001
2002,9,b2002
2003,16,b2003
2004,23,b2004
2005,30,b2005
2006,37,
2007,44,b2007
2008,1,b2008
2009,8,b2009
2010,15,b2010
2011,22,b2011
2012,29,b2012
2013,36,b2013
2014,43,b2014
2015,0,b2015
2016,7,b2016
2017,14,b2017
2018,21,b2018
2019,28,b2019
2020,35,b2020
2021,42,b2021
2022,-1,b2022
2023,6,
2024,13,b2024
2025,20,b2025
2026,27,b2026
2027,34,b2027
2028,41,b2028
2029,-2,b2029
2030,5,b2030
2031,12,b2031
2032,19,b2032
2033,26,b2033
2034,33,b2034
2035,40,b2035
2036,-3,b2036
2037,4,b2037
2038,11,b2038
2039,18,b2039
2040,25,
2041,32,b2041
2042,39,b2042
2043,-4,b2043
2044,3,b2044
2045,10,b2045
2046,17,b2046
2047,24,b2047
2048,31,b2048
2049,38,b2049
2050,-5,b2050
2051,2,b2051
2052,9,b2052
2053,16,b2053
2054,23,b2054
2055,30,b2055
2056,37,b2056
2057,44,
2058,1,b2058
2059,8,b2059
2060,15,b2060
2061,22,b2061
2062,29,b2062
2063,36,b2063
2064,43,b2064
2065,0,b2065
2066,7,b2066
2067,14,b2067
2068,21,b2068
2069,28,b2069
2070,35,b2070
2071,42,b2071
\.

\s2& INSERT INTO r9c71252 (id, a, b) VALUES (560, 0, 'w560') RETURNING id;

COMMIT;

\s2.;

DROP TABLE r9c71252log;

DROP TABLE r9c71252;

DROP FUNCTION r9c71252fb(), r9c71252fa(), r9c71252fs();

CREATE TABLE r9i77220 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i77220_1 PARTITION OF r9i77220 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i77220_2 PARTITION OF r9i77220 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i77220 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 156) i;

CREATE INDEX r9i77220_x0 ON r9i77220 USING gin (arr);

SET enable_seqscan = off;

SET enable_bitmapscan = off;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i77220 WHERE isempty(r);

-- statement
EXPLAIN (COSTS OFF) SELECT id, a FROM r9i77220 WHERE a < 41 ORDER BY a DESC NULLS FIRST, id LIMIT 10;
