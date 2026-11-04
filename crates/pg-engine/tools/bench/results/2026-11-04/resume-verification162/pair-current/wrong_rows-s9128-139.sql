-- class: wrong_rows
-- ordered: false
-- note: 11 vs 8 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Merge Append
-- engine:         Sort Key: ((r9i48100.p <-> '(3,3)'::point)), r9i48100.id
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i48100_1.p <-> '(3,3)'::point)), r9i48100_1.id
-- engine:               ->  Seq Scan on r9i48100_1
-- engine:                     Disabled: true
-- engine:         ->  Sort
-- engine:               Sort Key: ((r9i48100_2.p <-> '(3,3)'::point)), r9i48100_2.id
-- engine:               ->  Seq Scan on r9i48100_2
-- engine:                     Disabled: true
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Sort
-- reference:         Sort Key: ((r9i48100.p <-> '(3,3)'::point)), r9i48100.id
-- reference:         ->  Append
-- reference:               ->  Seq Scan on r9i48100_1
-- reference:                     Disabled: true
-- reference:               ->  Seq Scan on r9i48100_2
-- reference:                     Disabled: true
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a int2, b text, c text, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED) PARTITION BY RANGE (b);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO ('b');

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

INSERT INTO t0 (id, a, b, c) VALUES (36, 3, 'B', 'a_b'), (15, 3, 'ß', NULL), (25, 2, NULL, 'bar'), (20, NULL, NULL, 'a%'), (18, 3, 'ba', NULL), (23, NULL, NULL, '9'), (33, 3, 'A', 'ß'), (16, 6, 'Ä', NULL), (12, 1, 'B', 'ab'), (1, 2, 'é', 'a'), (26, 4, NULL, NULL), (35, NULL, 'Ä', NULL);

CREATE TABLE t1 (id int4, a date, b int8, c int8, d int8) PARTITION BY RANGE (a);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES FROM (MINVALUE) TO ('2024-02-01');

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

INSERT INTO t1 (id, a, b, c, d) VALUES (13, NULL, NULL, NULL, -1), (23, '2024-02-16', 12, 1, 3), (8, NULL, -2, NULL, NULL), (9, '2024-03-12', NULL, 8, NULL), (42, '2024-02-22', 10, 19, 2), (33, '2024-03-26', 5, 6, 2), (5, NULL, 18, 6, -1), (39, '2024-02-19', 18, 16, 17), (21, '2024-02-14', 15, NULL, 0), (22, '2024-02-15', 19, -2, 15), (37, '2024-02-17', 0, 2, NULL), (30, NULL, 12, 0, -2), (20, '2024-02-02', 10, 7, 5), (11, '2024-03-21', 14, NULL, 10), (34, '2024-01-10', NULL, 7, -2), (40, '2024-03-18', -1, NULL, 0), (6, '2024-03-25', 11, 11, 1), (4, '2024-02-23', 12, 1, 5), (45, '2024-03-24', 9, NULL, 8), (26, '2024-01-03', 18, NULL, 4), (57, '2024-03-28', 4, -2, 14), (16, '2024-03-05', 0, 4, -2), (14, '2024-02-07', 6, 3, 4), (58, '2024-02-07', 1, -2, 10), (36, '2024-03-13', 5, 6, 20), (1, NULL, -1, 15, 18), (25, '2024-01-26', 14, -1, 10), (27, '2024-02-07', 20, 5, 0), (12, '2024-02-21', 16, 1, 11), (46, '2024-02-05', 11, 19, 9);

CREATE INDEX t1_i411 ON t1 (d);

CREATE INDEX t1_i298 ON t1 (b);

CREATE INDEX t1_i897 ON t1 (a) WHERE a IS NOT NULL;

CREATE TABLE t2 (id int4, a bool, b numeric(10,2), g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2_c (id, a, b, extra) VALUES (227, NULL, NULL, 3);

INSERT INTO t2_c (id, a, b, extra) VALUES (898, true, NULL, 2);

INSERT INTO t2 (id, a, b) VALUES (28, NULL, 9.05);

CREATE INDEX t2_i664 ON t2 USING hash (a);

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, NULL, ARRAY[ROW('', 0)::ct3, ROW('abc', 2)::ct3], 2), (2, ROW(NULL, -1)::ct3, ARRAY[ROW('x ', 1)::ct3, ROW('é', -1)::ct3], NULL), (3, ROW('z', 2)::ct3, ARRAY[ROW('', 2)::ct3, ROW('x ', 3)::ct3], 3), (4, NULL, ARRAY[ROW('ba', 0)::ct3, ROW(NULL, 3)::ct3], 2), (5, ROW('a%', -1)::ct3, NULL, 0), (6, ROW(NULL, 3)::ct3, ARRAY[ROW('9', 2)::ct3, ROW('', 0)::ct3], 0), (7, ROW('9', NULL)::ct3, ARRAY[ROW('ab', 3)::ct3, ROW('10', 3)::ct3], 3), (8, NULL, '{}', 2), (9, ROW('abc', -1)::ct3, '{}', 4), (10, ROW(' x', 3)::ct3, ARRAY[ROW('b', 0)::ct3, ROW('b', 1)::ct3], 0), (11, ROW('z', -1)::ct3, NULL, 1);

CREATE INDEX tc3_ca ON tc3 (ca);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE STRICT AS $$ SELECT CASE WHEN (SELECT max(regexp_replace(q, '[ab]', 'y', 'g')) FROM (VALUES (12345678.9::numeric), ('NaN'::numeric), (NULL), (10.00::numeric)) s1(v0)) >= ANY (SELECT max(repeat(NULLIF('a,b', t4.c), 2)) FILTER (WHERE NULL::bool) AS c0 FROM t0 t3 RIGHT JOIN ONLY t0 t4 USING (b) GROUP BY t4.c HAVING ((SELECT every('{5}'::int4[] <> '{1,2}'::int4[]) FROM t2 t5)) IS UNKNOWN) THEN array_ndims('{{1,2},{3,4}}'::int4[]) ELSE jsonb_array_length('[]'::jsonb) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF substr(q, 0, 1) NOT IN (SELECT (COALESCE('a,b', q) COLLATE "default") FROM t0 t1 , t1 t2 WHERE (SELECT t1.c FROM t0 t3 ORDER BY 1 LIMIT 1) >= md5(t1.c) COLLATE "POSIX" GROUP BY GROUPING SETS ((t2.id), ())) THEN
    r := ascii((q || ' x'));
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(length(q), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2025-10-26 02:30:00', '2024-02-29', 'P1DT12H', '-1e-20', 'NaN', NULL), (2, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', 'infinity', NULL, '2025-01-01', 'P1DT12H', '-7', '-3', 4), (3, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2025-10-26 02:30:00', NULL, '23:59:59.999999', '-Infinity', NULL, 9), (4, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 5, "s": "", "ts": "2024-02-29"}', '0044-03-15 12:00:00+00 BC', '2024-02-29 00:00:00', '2000-03-01', '1 day', '99999999999999999999', '0.5', 10), (5, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 01:30:00-05', '2025-11-02 01:30:00', '-infinity', '-12:00', '-0.0', 'Infinity', 3), (6, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '-infinity', '2024-02-29 00:00:00', '2025-03-09', 'P1DT12H', '-1e-20', '0.5', 5), (7, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '2025-10-26 01:30:00+00', '2000-01-01 00:00:00', '-infinity', '-infinity', '-Infinity', 'NaN', 7), (8, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '1999-12-31 23:59:59.999999+00', NULL, '2000-03-01', '1 year 2 mons', '1e30', '0.125', 11), (9, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-04-06 02:30:00+11', NULL, '2025-01-01', '0', '-1e-20', 'Infinity', 10), (10, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', '0044-03-15 12:00:00+00 BC', '2025-03-09 02:30:00', NULL, '-1 year -1 day', '0.5', '1024', 2), (11, '"str"', '"str"', '2025-03-09 01:30:00-05', '2025-03-09 02:30:00', '2024-02-29', '0', '0.000001', NULL, 3);

INSERT INTO tp4 VALUES (5, '-infinity', 'b', '-0.0'), (0, '1999-12-31', 'ab', '0'), (12, '-infinity', 'ab', '-0.0'), (-4, '2025-01-01', 'b', '0');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "unicode", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 2, 'İ', '1e100', '1 day 00:00:00.000001', '[3,)', '{[0,100)}', NULL, NULL), (2, 1, 'ß', '2.5', '2147483647 days', '[0,10)', NULL, 'the and of', NULL), (3, 10, 'İ', NULL, '30 days', '[3,)', '{[1,3),[5,7)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(4, NULL)::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE INDEX r5a_iv ON r5a (iv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (NEW.g1 > 0) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 3 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 26;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (-1, 'a', 'infinity'), (16, 'b', 'infinity'), (6, 'c', 'infinity') ON CONFLICT DO NOTHING;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'Cid1', 1, NULL, '{"plan": "free", "tags": [1, 2]}'), (2, 'u2@x.org', 'ann2', 2, '{a}', '{"plan": "free", "tags": [1, 2]}'), (3, 'u3@x.org', 'bob 3', 0, '{a,b}', NULL), (4, 'u4@x.org', 'Éva4', 4, '{a,b}', '{"plan": "pro", "seats": 3}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (4, 9.99, 'paid', '2025-02-20', '{"items": []}'), (2, 9.99, 'new', '2025-02-12', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 12345678.90, 'void', '2025-02-01', '{"items": []}'), (3, 100, 'paid', '2025-02-18', '{"items": []}'), (2, 12345678.90, 'new', '2025-02-05', '{"gift": true}'), (3, -1.50, 'paid', '2025-02-10', '{"items": []}'), (2, 9.99, 'paid', '2025-02-16', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, 56, 'note 2', -12.125, NULL, '2025-03-09 06:59:59+00'), (2, 1, 168, 'n%5', 0, 1e300, '2025-03-09 06:59:59+00'), (3, 3, 120, '', NULL, 0.5, '2025-03-09 07:00:00+00'), (4, 2, 111, 'note_4', 0, 0.5, '2025-11-02 05:30:00+00'), (5, 1, 122, 'note 1', 3.333, 1e300, '2024-02-29 12:00:00+00'), (6, 1, 171, '', 3.333, NULL, '2024-02-29 12:00:00+00'), (7, 1, -41, '', NULL, -0, '2025-11-02 05:30:00+00'), (8, 1, 180, 'note 3', 0.5, NULL, NULL);

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

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

INSERT INTO r7t VALUES (1, 'Æ', 'Æ'), (2, 'a', 'a'), (3, '10', '10'), (4, '9', '9'), (5, 'ae', 'ae'), (6, 'a', 'a'), (7, '', ''), (8, 'e', 'e'), (9, 'ß', 'ß'), (10, 'ss', 'ss');

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

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, -1, 1.50, NULL, '2025-01-01'::timestamp + interval '86 hours'), (2, 4, 12, 1.50, 'a', '2025-01-01'::timestamp + interval '22 hours'), (3, 4, 7, 1.50, 'z', '2025-01-01'::timestamp + interval '82 hours'), (4, 2, 9, 1.50, 'ab', '2025-01-01'::timestamp + interval '16 hours'), (5, 3, 13, 3.75, 'b', '2025-01-01'::timestamp + interval '177 hours'), (6, 1, 10, 0, '', '2025-01-01'::timestamp + interval '39 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 6, 7), (1, 5, 7), (2, 1, 3), (2, 5, 2), (4, 3, 9), (4, 1, 4), (5, 3, 3), (5, 1, 6), (6, 1, 3), (6, 4, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-2,0)', '{}', 'empty', '[1.5,2.5)'), (2, 'empty', '{[6,6), [8, 10)}', '(2024-12-31,2025-01-05]', '(,0]'), (3, NULL, '{[5,8), [10, 12)}', '[2025-03-01,infinity)', NULL), (4, '[-2,-2)', NULL, 'empty', '(1.25,1.5)'), (5, '(-3,2]', '{}', '[2025-03-01,infinity)', '(,0]'), (6, '[8,8)', '{[8,8), [10, 12)}', '(2024-12-31,2025-01-05]', '(1.25,1.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, 'über café naïve'), (3, 'the quick brown fox'), (4, 'brown''s fox''s den'), (5, 'jumps over the lazy dog'), (6, 'a rat ran'), (7, 'a rat ran'), (8, 'jumps over the lazy dog');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3575) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2014) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2410) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r9a74260 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a74260 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 262, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 262) i;

ALTER TABLE r9a74260 ALTER COLUMN g SET STATISTICS 1;

SET default_statistics_target = 1;

CREATE STATISTICS r9a74260_st (mcv) ON (g + 1), b FROM r9a74260;

UPDATE r9a74260 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a74260 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a74260 WHERE g IN (1, 2, 3) AND b$$);

SELECT * FROM r9est($$SELECT * FROM r9a74260 WHERE g + 1 = 2$$);

SELECT * FROM r9est($$SELECT * FROM r9a74260 WHERE NOT b$$);

DROP TABLE r9a74260;

CREATE TABLE r9i79471 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i79471 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 227) i;

ANALYZE r9i79471;

CREATE UNIQUE INDEX r9i79471_x0 ON r9i79471 USING btree (t);

CREATE UNIQUE INDEX r9i79471_x1 ON r9i79471 USING btree (a);

REINDEX TABLE r9i79471;

SET enable_seqscan = on;

SET enable_bitmapscan = off;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i79471;

CREATE TABLE r9i41160 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i41160 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 238) i;

BEGIN;

CREATE INDEX r9i41160_x0 ON r9i41160 USING spgist (t);

CREATE INDEX ON r9i41160 USING btree (id) WITH (fillfactor = 50);

ROLLBACK;

INSERT INTO r9i41160 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(239, 384) i;

UPDATE r9i41160 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i41160 WHERE id % 11 = 0;

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i41160 ORDER BY p <-> point '(3,3)', id LIMIT 10;

EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i41160 WHERE r @> 7;

RESET enable_seqscan;

RESET enable_bitmapscan;

RESET enable_indexscan;

RESET enable_indexonlyscan;

DROP TABLE r9i41160;

\x2 name=r9x74023 types=23,23 describe rf=1 p=b00000001 p=t'1'
SELECT sum(v) FILTER (WHERE g = $1), count(*) FILTER (WHERE v IS NULL) FROM r9s WHERE id <= $2;

\x2 name=r9x74023 rows=1 p=b00000003 p=b000005db
-;

\x2 name=r9x74023 p=t'99' p=b7fffffff
-;

\x2 name=r9x74023 rf=1 p=b00000001 p=bfffffffb
-;

\x2 name=r9x74023 rf=0 p=t'3' p=t'50'
-;

\x2 name=r9x74023 rf=1 p=b00000008 p=b00000002
-;

\x2 name=r9x74023 rf=1,0 p=b00000002 p=b00000002
-;

\x2 name=r9x74023 p=t'1' p=t'2'
-;

\s2 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x74023';

\s2 DEALLOCATE r9x74023;

\s2 RESET plan_cache_mode;

PREPARE r9p22408 AS SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

EXECUTE r9p22408(3000000000);

EXECUTE r9p22408(7);

EXECUTE r9p22408(3000000000);

EXECUTE r9p22408(0);

EXECUTE r9p22408(3000000000);

EXECUTE r9p22408(-50);

EXPLAIN (COSTS OFF) EXECUTE r9p22408(7);

EXECUTE r9p22408(7);

EXECUTE r9p22408(NULL);

EXECUTE r9p22408(7);

EXECUTE r9p22408(NULL);

EXPLAIN (COSTS OFF) EXECUTE r9p22408(-1);

EXECUTE r9p22408(-1);

EXECUTE r9p22408(49);

EXECUTE r9p22408(NULL);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p22408';

DEALLOCATE r9p22408;

RESET plan_cache_mode;

\s2 START TRANSACTION READ WRITE;

\s2 INSERT INTO r7k (id, grp, v) VALUES (300, 2, 0) ON CONFLICT (id) DO UPDATE SET v = r7k.v + 1;

\s2 SAVEPOINT a;

\s2 DELETE FROM r7p WHERE id = 1;

\s2 INSERT INTO r7k (id, grp) VALUES (1, 1);

\s2 RELEASE a;

\s2 ROLLBACK TO a;

\s2 UPDATE r7k SET v = coalesce(v, 0) + 1 WHERE id = 2;

\s2 COMMIT AND CHAIN;

\s2 ROLLBACK;

CREATE TABLE r9w43451 (a int, b text);

INSERT INTO r9w43451 VALUES (1, 'x'), (2, 'yy'), (3, NULL);

PREPARE r9q43451 AS SELECT a, b FROM r9w43451 WHERE a >= $1 ORDER BY a;

EXECUTE r9q43451(1);

EXECUTE r9q43451(2);

ALTER TABLE r9w43451 ADD COLUMN c int;

EXECUTE r9q43451(1);

DEALLOCATE r9q43451;

DROP TABLE r9w43451;

VACUUM r9s;

EXPLAIN (COSTS OFF) SELECT id, v FROM r9s WHERE g IN (1, 3, 99) AND v < 55 ORDER BY id DESC LIMIT 3;

PREPARE r9p21069 (int, int) AS SELECT sum(v) FILTER (WHERE g = $1), count(*) FILTER (WHERE v IS NULL) FROM r9s WHERE id <= $2;

EXECUTE r9p21069(1, 2);

EXPLAIN (COSTS OFF) EXECUTE r9p21069(5, 2);

EXECUTE r9p21069(5, 2);

EXECUTE r9p21069(1, 777);

EXECUTE r9p21069(1, 777);

EXECUTE r9p21069(5, -5);

DISCARD PLANS;

EXECUTE r9p21069(1, 100);

EXECUTE r9p21069(5, 100);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p21069';

DEALLOCATE r9p21069;

RESET plan_cache_mode;

SET work_mem = '100kB';

RESET work_mem;

RESET enable_hashagg;

RESET enable_sort;

RESET enable_mergejoin;

RESET hash_mem_multiplier;

RESET enable_hashjoin;

CREATE TABLE r9i48100 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i48100_1 PARTITION OF r9i48100 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i48100_2 PARTITION OF r9i48100 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i48100 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 2158) i;

CREATE INDEX r9i48100_x0 ON r9i48100 USING brin (a) WITH (pages_per_range = 2);

INSERT INTO r9i48100 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(2159, 2196) i;

UPDATE r9i48100 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i48100 WHERE id % 11 = 0;

SET enable_seqscan = off;

-- statement
EXPLAIN (COSTS OFF) SELECT id, p <-> point '(3,3)' FROM r9i48100 ORDER BY p <-> point '(3,3)', id LIMIT 12;
