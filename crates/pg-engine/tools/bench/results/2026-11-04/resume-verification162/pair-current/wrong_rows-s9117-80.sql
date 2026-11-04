-- class: wrong_rows
-- ordered: false
-- note: 9 vs 6 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: ((r9i52627.p <-> '(3,3)'::point)), r9i52627.id
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i52627_1.p <-> '(3,3)'::point)), r9i52627_1.id
-- engine:               ->  Seq Scan on r9i52627_1
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i52627_2.p <-> '(3,3)'::point)), r9i52627_2.id
-- engine:               ->  Seq Scan on r9i52627_2
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: ((r9i52627.p <-> '(3,3)'::point)), r9i52627.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i52627_1
-- reference:               ->  Seq Scan on r9i52627_2
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a text, b int2, c int2);

INSERT INTO t0 (id, a, b, c) VALUES (27, NULL, NULL, 1), (28, 'z', 2, 6), (14, 'abc', 5, 3), (17, 'a,b', 6, 4), (6, NULL, 3, 2);

CREATE INDEX t0_i764 ON t0 (a, id);

CREATE INDEX t0_i261 ON t0 (a);

CREATE INDEX t0_i947 ON t0 (id, a);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a text[], b int4 DEFAULT -2);

INSERT INTO t1 (id, a, b) VALUES (21, NULL, 0), (13, NULL, NULL), (34, '{ab}', 9), (18, NULL, 6), (39, '{a,b}', 0), (20, NULL, 8), (8, '{}', -2), (26, '{A,a}', NULL), (45, NULL, NULL), (41, '{}', -1), (11, '{ab}', NULL), (31, '{z}', NULL), (25, NULL, 9), (19, NULL, 5), (6, '{}', 7), (35, NULL, NULL), (60, '{b,NULL}', NULL), (37, '{z}', NULL), (2, NULL, 6), (22, NULL, 3);

CREATE INDEX t1_i789 ON t1 (a);

CREATE INDEX t1_i466 ON t1 (b, id);

CREATE TABLE t2 (id int4 PRIMARY KEY, a date, b date, c d3int, d int8, e text, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

CREATE INDEX t2_i951 ON t2 (a, id);

CREATE TABLE t3 (a d3text, b text, c interval, d jsonb, e numeric(10,2)) PARTITION BY LIST (a);

CREATE TABLE t3_p0 PARTITION OF t3 FOR VALUES IN ('a', 'b', 'A');

CREATE TABLE t3_pd PARTITION OF t3 DEFAULT;

CREATE INDEX t3_i774 ON t3 USING gin (d);

CREATE INDEX t3_i197 ON t3 (d, e);

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, e FROM t2 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id, e FROM t2 WHERE id IS NULL;
  END IF;
END
$$;

CREATE VIEW v0 AS WITH w1 AS (SELECT max(NULL::timestamptz) AS c0, string_agg(reverse(t2.a), ',' ORDER BY reverse(t2.a) DESC) FILTER (WHERE true) AS c1, string_agg(CASE WHEN '2025-03-08'::date NOT IN ('2000-01-05'::date) THEN quote_literal(t2.a) ELSE ('2000-08-19 00:00:00+02:30'::timestamptz AT TIME ZONE 'EST5EDT')::text END, ',' ORDER BY CASE WHEN '2025-03-08'::date NOT IN ('2000-01-05'::date) THEN quote_literal(t2.a) ELSE ('2000-08-19 00:00:00+02:30'::timestamptz AT TIME ZONE 'EST5EDT')::text END DESC) AS c2 FROM t0 t2 WHERE true GROUP BY t2.b, t2.id HAVING false) SELECT '{"a": {"b": 2}}'::json AS c0, sum(s6.o) AS c1 FROM ((SELECT array_agg(NULLIF(t4.e, 'Ab') ORDER BY NULLIF(t4.e, 'Ab')) AS c0, justify_hours(('00:30:00'::interval * 9)) AS c1 FROM t2 t4 , t0 t5 WHERE (to_char(mod(1.5::numeric, 3), 'FM999.00') ~* '[[:upper:]]')) s3 JOIN unnest('{1,1,2}'::int4[]) WITH ORDINALITY s6(u, o) ON true) WHERE true;

CREATE VIEW v1 AS SELECT max(-3::int2) AS c0, (count(*))::int AS c1, 2::int2 AS c2 FROM (SELECT DISTINCT date_trunc('day', '2000-12-10 01:00:00+02:30'::timestamptz) AS c0, GREATEST(string_to_array(upper('foo'), ','), ARRAY[CASE WHEN NULL::bool THEN 'b' ELSE 'a,b' END, 'B']) AS c1, abs(abs('Infinity'::float8)) AS c2 FROM v0 t2 WHERE starts_with('a', CASE WHEN CASE WHEN false THEN NULL::bool ELSE true END THEN array_to_string('{ab}'::text[], ',', '?') ELSE 'a,b' END)) s1 HAVING true;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '1970-01-01 00:00:00.5', '2024-02-29', '-1 year -1 day', '-1e-20', '0.125', NULL), (2, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '-infinity', '2025-12-31 23:59:59.999999', '2000-03-01', '23:59:59.999999', NULL, '2', -2), (3, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '1970-01-01 00:00:00.5', '4713-01-01 BC', NULL, NULL, '-0', 3), (4, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', NULL, '2024-02-29 00:00:00', '4713-01-01 BC', '-infinity', NULL, 'Infinity', 8), (5, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '2025-11-02 01:30:00-04', '2024-02-29 00:00:00', '2000-03-01', 'P1DT12H', '2.5', '-0', 8);

INSERT INTO tp4 VALUES (-5, 'infinity', 'd', '-0.0'), (-3, '1999-12-31', 'ab', 'Infinity'), (7, '-infinity', 'ab', NULL), (19, '1999-12-31', 'c', '1e30'), (2, '2000-03-01', 'a', '-1e-20'), (10, '1999-12-31', 'b', NULL), (19, '-infinity', 'a', '2.5'), (-2, '2025-03-09', 'b', 'NaN');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "unicode", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 9, 'I', 'Infinity', '178000000 years', '[-3,0]', '{}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(NULL, 'c')::r5ct, ROW(3, 'a')::r5ct]), (2, 17, NULL, '2.5', NULL, '[0,10)', '{[0,100)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(4, 'B')::r5ct, ROW(-2, 'h')::r5ct]), (3, 3, 'SS', 'NaN', '-1 mon 30 days', '(,2]', '{[1,2],[2,3)}', '', '{}'), (4, NULL, ' a', '-1e40', '2147483647 days', '[3,)', '{}', 'The quick brown fox jumps over the lazy dog', NULL), (5, 11, 'Z', '0.0000000000000000000000000001', '2147483647 days', '(,2]', '{[1,3),[5,7)}', 'the and of', ARRAY[ROW(NULL, 'İ')::r5ct, ROW(3, 'a-')::r5ct, ROW(3, 'ö')::r5ct]), (6, 20, '', '-1e40', '1 mon', '(,2]', '{empty}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', NULL);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 29 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (9, 'a', '2024-02-29'), (19, 'b', '2025-01-01'), (9, 'c', '2024-02-29'), (2, 'd', '2024-02-29'), (-3, 'a', '2024-02-29'), (10, 'b', '2024-02-29'), (14, 'c', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (32, 'x0', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Éva1', 1, '{a}', NULL), (2, 'u2@x.org', 'bob2', 2, '{a}', '{}'), (3, 'u3@x.org', '3', 3, '{a}', '{"seats": 1.50}'), (4, 'u4@x.org', 'ann4', 0, NULL, NULL), (5, 'u5@x.org', 'Éva5', 5, '{}', '[1, {"a": null}]'), (6, 'u6@x.org', 'bob 6', 6, '{a,b}', '{"seats": 1.50}'), (7, 'u7@x.org', 'zoë7', 7, '{}', NULL), (8, 'u8@x.org', '8', 0, '{}', '{"seats": 1.50}'), (9, 'u9@x.org', 'Éva9', 9, '{}', '{"seats": 1.50}'), (10, 'u10@x.org', 'bob 10', 10, '{a,b}', '{"seats": 1.50}'), (11, 'u11@x.org', 'ann11', 0, '{b,NULL}', NULL), (12, 'u12@x.org', 'Éva12', 12, '{a,b}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 12345678.90, 'paid', '2025-02-27', '{"gift": true}'), (2, -1.50, 'void', '2025-02-14', NULL), (10, NULL, 'void', '2025-02-08', '{"items": []}'), (7, 100, 'paid', '2025-02-03', NULL), (2, NULL, 'void', '2025-02-20', '{"items": []}'), (10, -1.50, 'void', '2025-02-04', '{"gift": true}'), (11, 100, 'paid', '2025-02-05', '{"items": []}'), (11, 100, 'void', '2025-02-20', '{"items": [{"sku": "a", "qty": 2}]}'), (1, NULL, 'paid', '2025-02-27', '{"items": []}'), (6, 12345678.90, 'paid', '2025-02-21', '{"gift": true}'), (6, NULL, 'new', '2025-02-25', '{"gift": true}'), (11, 100, 'paid', '2025-02-03', '{"gift": true}'), (1, 0, 'void', '2025-02-24', '{"gift": true}'), (12, NULL, 'void', '2025-02-01', '{"gift": true}'), (5, 100, 'paid', '2025-02-24', '{"items": []}'), (5, 0, 'paid', '2025-02-06', NULL);

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

INSERT INTO r7k VALUES (1, 2, 94, 'note 3', 0, 'NaN', '2025-11-02 05:30:00+00'), (2, 4, 119, 'note 1', 1000, 'NaN', NULL), (3, 1, 86, 'Note 3', 0, 0.5, NULL), (4, 7, 139, 'note_4', -12.125, 'NaN', NULL), (5, 6, 198, 'NOTE 3', 1000, 1e300, '2025-11-02 05:30:00+00'), (6, 1, 62, 'note 2', 0.5, 2.25, '2025-11-02 05:30:00+00'), (7, 1, -27, 'n%5', 0.5, NULL, NULL), (8, 1, NULL, '', 0.5, 'NaN', '2024-02-29 12:00:00+00'), (9, 1, -22, 'note 3', NULL, -0, '2025-11-02 05:30:00+00');

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

CREATE TABLE r7g (id int GENERATED BY DEFAULT AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, 'A', 'A'), (2, 'Ä', 'Ä'), (3, 'ä b', 'ä b'), (4, 'ae', 'ae'), (5, 'ä', 'ä'), (6, 'ß', 'ß'), (7, 'e', 'e'), (8, 'Ä', 'Ä'), (9, 'ß', 'ß');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, NULL, 0, '', '2025-01-01'::timestamp + interval '32 hours'), (2, 2, NULL, NULL, '', '2025-01-01'::timestamp + interval '18 hours'), (3, 3, 6, 3.75, NULL, '2025-01-01'::timestamp + interval '12 hours'), (4, 4, 17, -2.25, 'B', '2025-01-01'::timestamp + interval '129 hours'), (5, 3, 10, 1.50, 'b', '2025-01-01'::timestamp + interval '68 hours'), (6, 3, -5, 1.50, 'b', '2025-01-01'::timestamp + interval '52 hours'), (7, 2, 4, NULL, 'B', '2025-01-01'::timestamp + interval '66 hours'), (8, 4, NULL, 0, 'B', '2025-01-01'::timestamp + interval '4 hours'), (9, 3, 6, 1.50, '', '2025-01-01'::timestamp + interval '42 hours'), (10, 2, 1, -2.25, 'B', '2025-01-01'::timestamp + interval '48 hours'), (11, 3, 11, 1.50, 'a', '2025-01-01'::timestamp + interval '11 hours'), (12, 4, 10, NULL, NULL, '2025-01-01'::timestamp + interval '28 hours'), (13, 3, NULL, 10, 'B', '2025-01-01'::timestamp + interval '96 hours'), (14, 1, NULL, NULL, 'z', '2025-01-01'::timestamp + interval '30 hours'), (15, 2, 0, 3.75, 'b', '2025-01-01'::timestamp + interval '44 hours'), (16, 4, 2, 3.75, NULL, '2025-01-01'::timestamp + interval '32 hours'), (17, 4, -3, -2.25, 'a', '2025-01-01'::timestamp + interval '168 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 2, 4), (2, 3, 5), (3, 6, 3), (3, 7, 5), (4, 2, 9), (5, 3, 2), (6, 4, 4), (7, 3, 1), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[6,)', '{}', NULL, '(,0]'), (2, NULL, '{[3,7), [9, 11)}', 'empty', '(1.25,1.5)'), (3, '[5,)', '{[5,9), [11, 13)}', '[2025-03-01,infinity)', '(,0]'), (4, NULL, '{(,3)}', '[2025-01-01,2025-02-01)', '[1.5,2.5)'), (5, NULL, '{(,4)}', '[2025-03-01,infinity)', '[0,0]'), (6, '[3,4)', '{(,3)}', 'empty', '[1.5,2.5)'), (7, NULL, '{(,4)}', '[2025-01-01,2025-02-01)', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'the quick brown fox'), (2, 'quick quick slow'), (3, 'über café naïve'), (4, 'brown''s fox''s den'), (5, 'running rats'), (6, 'quick quick slow'), (7, 'foxes and dogs'), (8, 'foxes and dogs'), (9, 'quick quick slow'), (10, 'jumps over the lazy dog');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (3, '"scalar"'), (4, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3733) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1588) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3565) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r9c71155 (id int PRIMARY KEY, a int, b text) WITH (autovacuum_enabled = off);

CREATE TABLE r9c71155log (n serial, op text, id int, b text);

CREATE FUNCTION r9c71155fb() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN IF NEW.a < -3 THEN RETURN NULL; END IF;  NEW.b := upper(NEW.b); RETURN NEW; END $f$;

CREATE FUNCTION r9c71155fa() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c71155log (op, id, b) VALUES (TG_WHEN || ' ' || TG_LEVEL, NEW.id, NEW.b); RETURN NULL; END $f$;

CREATE FUNCTION r9c71155fs() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN INSERT INTO r9c71155log (op, id, b) SELECT 'stmt', count(*), max(b) FROM nt; RETURN NULL; END $f$;

CREATE TRIGGER r9c71155s AFTER INSERT ON r9c71155 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r9c71155fs();

BEGIN;

COPY r9c71155 (id, a, b) FROM STDIN;
1	2	b1
2	9	b2
3	16	b3
4	23	b4
5	30	b1
6	37	b6
7	44	b7
8	1	b8
9	8	b9
10	15	b10
\.

\s2& INSERT INTO r9c71155 (id, a, b) VALUES (5, 0, 'w5') RETURNING id;

COMMIT;

\s2.;

DROP TABLE r9c71155log;

DROP TABLE r9c71155;

DROP FUNCTION r9c71155fb(), r9c71155fa(), r9c71155fs();

CREATE TABLE r8t9023 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t9023 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf9023() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t9023', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf9023_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t9023', r);
  RETURN NULL;
END $$;

CREATE TRIGGER e_tr BEFORE TRUNCATE ON r8t9023 FOR EACH STATEMENT EXECUTE FUNCTION r8tf9023();

CREATE TRIGGER e_tr BEFORE TRUNCATE ON r8t9023 FOR EACH STATEMENT EXECUTE FUNCTION r8tf9023();

CREATE TRIGGER b_air AFTER INSERT OR DELETE ON r8t9023 FOR EACH ROW EXECUTE FUNCTION r8tf9023();

CREATE TRIGGER c_bus BEFORE UPDATE OR DELETE ON r8t9023 FOR EACH STATEMENT EXECUTE FUNCTION r8tf9023('stmt');

INSERT INTO r8t9023 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t9023.v RETURNING *;

DELETE FROM r8log WHERE tg = 't9023';

DELETE FROM r8t9023 WHERE id = 3 RETURNING *;

DELETE FROM r8log WHERE tg = 't9023';

UPDATE r8t9023 SET v = coalesce(v, 0) - 1 RETURNING id, v;

DELETE FROM r8log WHERE tg = 't9023';

INSERT INTO r8t9023 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't9023';

DROP TABLE r8t9023;

DROP FUNCTION r8tf9023, r8tf9023_tt;

SELECT lo_from_bytea(959297, '\x41424344454647')::int8 - 959297;

SELECT lo_get(959297), lo_get(959297, 0, 2);

BEGIN;

SELECT lo_open(959297, x'20000'::int);

SELECT lo_lseek(0, 0, 3);

SELECT lo_tell(0), lo_lseek64(0, 0, 2);

SELECT lo_truncate(0, 5);

SELECT lo_close(0);

ROLLBACK;

SELECT lo_get(959297);

SELECT lo_put(959297, 13, '\x7171');

SELECT lo_get(959297), length(lo_get(959297));

SELECT oid::int8 - 959297, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = 959297;

SELECT loread(7, 1);

SELECT lo_unlink(959297);

SELECT lo_get(959297);

\s2 BEGIN;

\s2 UPDATE r7k SET v = coalesce(v, 0) + 10, grp = grp + 1 WHERE id = 4;

\s3 BEGIN;

\s3& SELECT id, v FROM r7k WHERE id = 4 FOR NO KEY UPDATE;

\s2 COMMIT;

\s3.;

\s3 COMMIT;

\s2 ROLLBACK;

\s3 ROLLBACK;

CREATE TABLE r9i52627 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i52627_1 PARTITION OF r9i52627 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i52627_2 PARTITION OF r9i52627 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i52627 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 2450) i;

CREATE INDEX ON r9i52627 USING gist (r);

CREATE INDEX r9i52627_x1 ON r9i52627 USING btree (a) INCLUDE (t);

INSERT INTO r9i52627 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(2451, 2589) i;

UPDATE r9i52627 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i52627 WHERE id % 11 = 0;

SET enable_seqscan = on;

-- statement
EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i52627 ORDER BY p <-> point '(3,3)', id LIMIT 10;
