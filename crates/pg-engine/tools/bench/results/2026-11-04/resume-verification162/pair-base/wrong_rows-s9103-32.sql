-- class: wrong_rows
-- ordered: false
-- note: 9 vs 6 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: ((r9i59895.p <-> '(3,3)'::point)), r9i59895.id
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i59895_1.p <-> '(3,3)'::point)), r9i59895_1.id
-- engine:               ->  Seq Scan on r9i59895_1
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i59895_2.p <-> '(3,3)'::point)), r9i59895_2.id
-- engine:               ->  Seq Scan on r9i59895_2
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: ((r9i59895.p <-> '(3,3)'::point)), r9i59895.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i59895_1
-- reference:               ->  Seq Scan on r9i59895_2
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

-- statement
EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i59895 ORDER BY p <-> point '(3,3)', id LIMIT 5;
