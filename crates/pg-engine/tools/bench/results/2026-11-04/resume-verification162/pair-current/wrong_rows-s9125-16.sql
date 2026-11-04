-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: QUERY PLAN:25
-- engine: Index Scan using r9s_pkey on r9s
-- engine:   Index Cond: (id = ANY ('{1,2,3}'::integer[]))
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Index Scan using r9s_pkey on r9s
-- reference:   Index Cond: (id = ANY (($1)::integer[]))
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a float8, b text COLLATE "C");

INSERT INTO t0 (id, a, b) VALUES (35, 3, 'Ä');

CREATE INDEX t0_i111 ON t0 (id) WHERE id IS NOT NULL;

CREATE INDEX t0_i823 ON t0 (b) WHERE b IS NOT NULL;

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (a timestamptz, b int4[], c int4[], d date);

CREATE TABLE t1_c (extra int) INHERITS (t1);

INSERT INTO t1_c (a, b, c, d, extra) VALUES ('2024-01-08 00:00:00+00', '{1,2}', NULL, NULL, 1);

INSERT INTO t1_c (a, b, c, d, extra) VALUES ('2024-01-05 12:00:00+00', '{}', '{3,1,2}', NULL, 0);

INSERT INTO t1 (a, b, c, d) VALUES ('2024-01-04 12:00:00+00', NULL, NULL, NULL), ('2024-01-02 06:00:00+00', '{1,2}', NULL, '2024-02-17'), (NULL, NULL, '{1,1,2}', '2024-01-10'), ('2024-01-04 00:00:00+00', '{1,1,2}', '{2}', NULL), ('2024-01-02 23:00:00+00', NULL, '{5}', '2024-03-13'), ('2024-01-04 06:00:00+00', '{2}', '{1,1,2}', '2024-03-14'), ('2024-01-01 12:00:00+00', NULL, NULL, '2024-02-26'), ('2024-01-06 12:00:00+00', '{1,1,2}', '{3,1,2}', '2024-03-12'), ('2024-01-07 00:00:00+00', '{}', NULL, NULL), (NULL, NULL, '{2}', NULL), ('2024-01-04 00:00:00+00', '{}', '{}', NULL), ('2024-01-05 12:00:00+00', NULL, NULL, '2024-03-11');

CREATE INDEX t1_i243 ON t1 (d);

CREATE INDEX t1_i374 ON t1 USING hash (b);

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('aa', 0)::ct3, '{}', 0), (2, ROW('B', 0)::ct3, NULL, NULL), (3, NULL, '{}', 4), (4, ROW('Ab', -1)::ct3, '{}', 0), (5, ROW('A', 1)::ct3, NULL, 2), (6, ROW('ß', 0)::ct3, ARRAY[ROW('b', 0)::ct3, ROW('ba', 0)::ct3], 4), (7, ROW('Z', 0)::ct3, ARRAY[ROW('ba', 3)::ct3, ROW('Ab', 2)::ct3], 3), (8, ROW('9', -1)::ct3, ARRAY[ROW('a', 2)::ct3, ROW('foo', 0)::ct3], 2), (9, ROW('abc', NULL)::ct3, NULL, 4);

CREATE INDEX tc3_c ON tc3 (c);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := p;
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

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN upper(q) = NULLIF(q, q) COLLATE "und-x-icu" THEN p ELSE ((p)::int8)::int4 END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE sql STABLE AS $$ SELECT id, b FROM t0 WHERE id <= lim $$;

CREATE VIEW v0 AS SELECT (COALESCE('{2,NULL}'::int4[], t1.b))[1:2] AS c0, min(-1::int2) AS c1, array_agg(DISTINCT COALESCE(8, 12) ORDER BY COALESCE(8, 12) NULLS FIRST) AS c2, GROUPING(t1.b, t1.a, ((1 - 12))) FROM t1 t1 WHERE (trim(upper('aa' COLLATE "und-x-icu")) ILIKE '%B_') GROUP BY GROUPING SETS ((t1.b), (t1.a));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, 'null', 'null', '2024-02-29 23:00:00+00', '0001-01-01 00:00:00 BC', '2025-01-01', '1 mon -30 days', 'Infinity', '-0', 4), (2, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-03-09 01:30:00-05', '2000-01-01 00:00:00', '2024-02-29', '-infinity', '-2.5', '-1.25', 11), (3, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-04-06 02:30:00+11', '0001-01-01 00:00:00 BC', '2025-01-01', '1 year 2 mons', '2.5', 'NaN', 9), (4, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-10-26 01:30:00+00', '2025-11-02 01:30:00', '2025-01-01', '23:59:59.999999', '123.45', '-3', 5), (5, NULL, NULL, 'infinity', '0001-01-01 00:00:00 BC', 'infinity', '-1 mon 2 days', '-7', '1024', NULL), (6, '{}', '{}', 'infinity', '2025-10-26 02:30:00', '-infinity', '23:59:59.999999', 'Infinity', '-0', 1);

INSERT INTO tp4 VALUES (6, '4713-01-01 BC', 'a', '-0.0'), (22, '2025-01-01', 'c', '-1e-20'), (6, '-infinity', 'b', '2.5'), (17, 'infinity', 'd', '1e30'), (16, '4713-01-01 BC', 'b', '-1e-20'), (-2, '-infinity', 'd', '99999999999999999999'), (8, '-infinity', 'd', '123.45'), (7, '-infinity', 'c', '0.5'), (13, 'infinity', 'b', '99999999999999999999'), (10, 'infinity', 'ab', '-1e-20'), (11, '4713-01-01 BC', 'a', '0.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "pg_c_utf8", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 12, 'ö', 'Infinity', '1 mon', '(,)', '{empty}', '', ARRAY[ROW(3, '9')::r5ct, ROW(0, 'Ä')::r5ct, ROW(3, 'a-')::r5ct]), (2, 17, 'B', '123456789012345678901234567890.123456789', NULL, '[5,6)', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(4, 'Ab')::r5ct]), (3, 14, 'c', '2.5', '1 mon', '[0,10)', '{[0,100)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(1, 'İ')::r5ct, ROW(4, '2')::r5ct, ROW(4, 'ı')::r5ct]), (4, 8, 'ö', '0.0000000000000000000000000001', 'infinity', '[5,6)', '{}', NULL, '{}'), (5, 10, 'c', '123456789012345678901234567890.123456789', '-2147483648 days', '[-3,0]', '{[1,2],[2,3)}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', ARRAY[ROW(2, 'z')::r5ct, ROW(-1, 'ae')::r5ct, ROW(4, 'SS')::r5ct]), (6, 5, 'ö', '0.0000000000000000000000000001', '1 mon', '[1,5)', '{empty}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(0, 'i')::r5ct, ROW(3, NULL)::r5ct, ROW(0, 'ab')::r5ct]), (7, -3, '10', NULL, '1 year -1 sec', '[3,)', '{[0,100)}', 'running runners ran; the runner runs', NULL), (8, 12, 'a-', '0', '-infinity', '[0,10)', '{[1,3),[5,7)}', 'running runners ran; the runner runs', ARRAY[ROW(5, 'E')::r5ct, ROW(NULL, 'ß')::r5ct, ROW(0, 'ß')::r5ct]), (9, 1, 'a-', '-Infinity', '2147483647 mons', '(,2]', '{[1,2],[2,3)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(NULL, 'b')::r5ct, ROW(-2, 'ab')::r5ct, ROW(5, 'B')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (NEW.g1 > 0) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 1 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 10;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (12, 'a', '2024-02-29'), (8, 'b', '2024-02-29'), (9, 'c', '2024-02-29'), (5, 'd', 'infinity'), (8, 'a', '2024-02-29'), (15, 'b', '2024-02-29'), (5, 'c', 'infinity'), (10, 'd', '2024-02-29') ON CONFLICT DO NOTHING;

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Cid1', 1, NULL, NULL), (2, 'u2@x.org', 'bob2', 0, '{a}', '{"plan": "free", "tags": [1, 2]}'), (3, 'u3@x.org', 'ann3', 3, '{}', NULL), (4, 'u4@x.org', 'zoë4', 4, NULL, '{"plan": "pro", "seats": 3}'), (5, 'u5@x.org', '5', 5, '{a,b}', NULL), (6, 'u6@x.org', 'bob6', 0, '{b,NULL}', '[1, {"a": null}]'), (7, 'u7@x.org', 'ann7', 7, '{}', '{"plan": "free", "tags": [1, 2]}'), (8, 'u8@x.org', '8', 8, '{}', '{}'), (9, 'u9@x.org', 'Éva9', 0, NULL, '{"plan": "pro", "seats": 3}'), (10, 'u10@x.org', 'Cid10', 10, '{}', '{}'), (11, 'u11@x.org', 'ann11', 0, '{a,b}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, -1.50, 'void', '2025-02-25', '{"gift": true}'), (4, 100, 'paid', '2025-02-08', '{"gift": true}'), (10, NULL, 'new', '2025-02-21', '{"gift": true}'), (11, 100, 'void', '2025-02-06', NULL);

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

INSERT INTO r7k VALUES (1, 1, 60, 'NOTE 3', -12.125, 'NaN', '2025-03-09 07:00:00+00'), (2, 1, -35, 'note 1', 0.5, -0, '2025-03-09 06:59:59+00'), (3, 1, 159, 'NOTE 3', NULL, 0.5, '2025-03-09 06:59:59+00'), (4, 1, 10, 'Note 3', 0, NULL, '2024-02-29 12:00:00+00'), (5, 1, 140, 'Note 3', 1000, -0, '2024-02-29 12:00:00+00'), (6, 1, 70, 'é', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (7, 1, 173, 'note 1', 0, 2.25, NULL), (8, 1, 167, 'é', 1000, 0.5, NULL), (9, 8, 100, 'note_4', 1000, 'Infinity', '2024-02-29 12:00:00+00'), (10, 2, 0, '', -12.125, 0.5, '2025-03-09 07:00:00+00'), (11, 5, 9, 'é', -12.125, 'NaN', '2025-03-09 07:00:00+00'), (12, 7, 53, 'n%5', 0, 1e300, NULL), (13, 1, 65, 'Note 3', -12.125, 'NaN', '2024-02-29 12:00:00+00'), (14, 1, -14, 'note 2', 1000, NULL, '2024-02-29 12:00:00+00'), (15, 4, 40, 'note 3', 0.5, 0.5, '2025-03-09 06:59:59+00'), (16, 1, 84, 'NOTE 3', 3.333, 2.25, '2025-03-09 06:59:59+00'), (17, 1, 174, 'note 2', NULL, 0.5, '2025-11-02 05:30:00+00'), (18, 3, 172, 'note 2', NULL, NULL, '2024-02-29 12:00:00+00'), (19, 6, 140, 'note 3', NULL, 'NaN', '2025-03-09 07:00:00+00'), (20, 1, 173, 'note 2', -12.125, 'NaN', '2025-11-02 05:30:00+00'), (21, 1, 71, 'note 1', -12.125, 0.5, '2025-11-02 05:30:00+00'), (22, 1, 112, 'note 2', -12.125, 2.25, '2025-03-09 06:59:59+00'), (23, 1, 22, 'note 1', -12.125, 'NaN', NULL), (24, 1, 127, 'note 1', 0, 2.25, '2025-11-02 05:30:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10);

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

INSERT INTO r7t VALUES (1, 'z', 'z'), (2, 'ä b', 'ä b'), (3, '9', '9'), (4, 'e', 'e'), (5, 'z', 'z');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 2, 5, 1.50, NULL, '2025-01-01'::timestamp + interval '36 hours'), (2, 2, 13, 1.50, '', '2025-01-01'::timestamp + interval '61 hours'), (3, 4, 2, 3.75, NULL, '2025-01-01'::timestamp + interval '28 hours'), (4, 1, 5, NULL, 'z', '2025-01-01'::timestamp + interval '34 hours'), (5, 2, 18, 0, 'a', '2025-01-01'::timestamp + interval '23 hours'), (6, 1, -4, 3.75, 'B', '2025-01-01'::timestamp + interval '60 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (2, 1, 7), (2, 2, 4), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{}', '[2025-03-01,infinity)', '[0,0]'), (2, NULL, '{[8,12), [14, 16)}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (3, NULL, '{}', '[2025-01-01,2025-02-01)', NULL), (4, '[1,)', '{}', 'empty', '(1.25,1.5)'), (5, '[3,5)', '{}', '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (6, NULL, '{(,6)}', '[2025-03-01,infinity)', '[1.5,2.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, 'running rats'), (3, 'über café naïve'), (4, 'foxes and dogs'), (5, 'dog-eat-dog world'), (6, ''), (7, 'a rat ran'), (8, 'jumps over the lazy dog'), (9, 'running rats'), (10, '');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (4, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3752) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2209) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2286) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

EXPLAIN (COSTS OFF) SELECT id, t FROM r9s WHERE t COLLATE "en-x-icu" BETWEEN 'a' AND 'b' ORDER BY t COLLATE "en-x-icu", id LIMIT 24;

MERGE INTO r7d t USING (VALUES (11, 150), (1, 7), (10, 7), (8, 150)) s(id, v) ON t.id = s.id WHEN MATCHED AND s.v = 0 THEN DO NOTHING WHEN MATCHED THEN UPDATE SET p = s.v, ps = ARRAY[s.v] WHEN NOT MATCHED THEN INSERT (id, p, code) VALUES (s.id, s.v, 'QQ' || (s.id % 10)) RETURNING merge_action(), t.*;

PREPARE r9p44445 (text) AS SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;

EXPLAIN (COSTS OFF) EXECUTE r9p44445('{}');

EXECUTE r9p44445('{}');

EXECUTE r9p44445('{}');

EXECUTE r9p44445('{2999,1}');

EXECUTE r9p44445('{}');

EXECUTE r9p44445('{50,100,NULL}');

EXECUTE r9p44445('{50,100,NULL}');

EXECUTE r9p44445('{2999,1}');

-- statement
EXPLAIN (COSTS OFF) EXECUTE r9p44445('{1,2,3}');
