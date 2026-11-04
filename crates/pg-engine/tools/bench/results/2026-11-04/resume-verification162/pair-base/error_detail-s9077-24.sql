-- class: error_detail
-- ordered: false
-- note: 42P20
-- engine: ERROR 42P20: cannot copy window "w" because it has a frame clause
-- engine: HINT: Omit the parentheses in this OVER clause.
-- engine: POSITION: 20
-- reference: ERROR 42P20: cannot copy window "w" because it has a frame clause
-- reference: POSITION: 20

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a float8, b text COLLATE "POSIX", c text, d numeric(10,2), e text);

INSERT INTO t0 (id, a, b, c, d, e) VALUES (17, NULL, NULL, NULL, 6.05, 'bar'), (38, 4, NULL, NULL, NULL, NULL), (8, NULL, 'b', 'b', 0.00, NULL), (37, NULL, 'abc', NULL, 12.50, NULL), (27, 3, NULL, NULL, NULL, NULL), (11, NULL, 'B', NULL, NULL, NULL), (31, 3, '10', 'a,b', 4.50, 'B'), (20, NULL, NULL, 'abc', NULL, 'aa'), (12, NULL, NULL, NULL, NULL, NULL), (3, NULL, NULL, NULL, NULL, 'a'), (28, NULL, NULL, NULL, NULL, NULL), (29, NULL, NULL, NULL, NULL, NULL);

CREATE INDEX t0_i51 ON t0 (id);

CREATE TABLE t1 (a date, b float8, c numeric(10,2), d timestamp, e int4);

INSERT INTO t1 (a, b, c, d, e) VALUES ('2024-01-06', 0.25, NULL, NULL, 2), ('2024-01-01', -0.75, 6.05, '2024-01-07 23:30:00', NULL), ('2024-02-01', NULL, 12.25, '2024-01-08 00:00:00', NULL), (NULL, NULL, NULL, '2024-01-08 00:30:00', 0), (NULL, 0.5, 5.50, '2024-01-08 06:00:00', 0), ('2024-03-12', 2.75, NULL, '2024-01-04 23:00:00', NULL), (NULL, 0.5, NULL, '2024-01-02 06:30:00', NULL), (NULL, 3.5, 8.05, '2024-01-08 12:30:00', NULL), (NULL, NULL, -2.00, '2024-01-01 23:00:00', 5), (NULL, 3.25, NULL, NULL, 9), ('2024-01-16', 4, -2.25, NULL, 0), ('2024-02-06', -1.5, NULL, '2024-01-02 06:30:00', -1), ('2024-02-03', 1.75, 2.00, NULL, NULL), ('2024-03-11', NULL, NULL, NULL, 3), ('2024-02-21', NULL, 8.50, '2024-01-04 00:00:00', 0), (NULL, NULL, NULL, NULL, NULL), (NULL, NULL, 3.00, '2024-01-03 12:30:00', 9), ('2024-02-28', 3.25, NULL, '2024-01-08 00:30:00', 0), (NULL, -0.75, NULL, NULL, NULL), (NULL, -0.5, 11.25, '2024-01-06 23:30:00', NULL), (NULL, NULL, 0.00, '2024-01-06 00:30:00', NULL), ('2024-01-04', NULL, 9.25, NULL, NULL), ('2024-01-01', NULL, NULL, '2024-01-03 12:30:00', NULL), (NULL, 0.25, 11.05, '2024-01-06 12:00:00', NULL), (NULL, NULL, NULL, '2024-01-08 00:30:00', NULL), ('2024-02-08', NULL, 3.50, NULL, -2), ('2024-02-28', NULL, 9.50, NULL, 1), (NULL, -0.5, NULL, NULL, 3), ('2024-02-22', NULL, 9.25, NULL, NULL), (NULL, -0.5, 3.05, '2024-01-06 06:00:00', NULL);

CREATE INDEX t1_i705 ON t1 (c, b);

CREATE INDEX t1_i309 ON t1 (b);

CREATE INDEX t1_i935 ON t1 (d) WHERE d IS NOT NULL;

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT CASE WHEN false THEN p ELSE strpos(split_part(q, ',', 1), 'a') END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  AS $$
DECLARE r int := 0;
BEGIN
  IF true THEN
    r := p;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(cardinality(('{}'::int4[])[1:2]), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT NULLIF(0::numeric, round((3::numeric + -7.125::numeric), 1)) AS c0, abs((s1.g)::int8) NOT IN (SELECT DISTINCT NULLIF(9::int8, (abs(5::int8) * upper(int4range(LEAST(s1.g, t2.id), GREATEST(s1.g, t2.id))))) FROM t0 t2) AS c1, LEAST(reverse(md5(' x')), 'é') AS c2, '{"a": null}'::jsonb AS c3 FROM generate_series(1, 1) s1(g);

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '1970-01-01 00:00:00.5', '2025-03-09', 'P1DT12H', NULL, '1024', -3), (2, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '2025-03-09 01:30:00-05', '2024-02-29 00:00:00', '2024-02-29', '1 day', NULL, 'NaN', 3), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '1970-01-01 00:00:00.5', '2000-03-01', '-1 year -1 day', 'NaN', '-0', 6), (4, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2000-01-01 00:00:00', NULL, '1 year 2 mons', '1e30', 'Infinity', 3), (5, '"str"', '"str"', '-infinity', '1970-01-01 00:00:00.5', '-infinity', 'infinity', '0', NULL, 4), (6, NULL, '{"a": 1, "a": 2,  "b" : [1,2]}', '0044-03-15 12:00:00+00 BC', '2024-02-29 00:00:00', '4713-01-01 BC', NULL, NULL, '-0', -1), (7, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '2025-03-09 01:30:00-05', 'infinity', '2000-03-01', '-1 year -1 day', '2.5', '-1.25', NULL), (8, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', NULL, '2024-02-29', '-12:00', 'NaN', '0.5', -3), (9, '{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 00:30:00+00', NULL, '4713-01-01 BC', '0', '-7', '0.125', 9);

INSERT INTO tp4 VALUES (22, '2025-01-01', 'ab', '1.23456789012345678901234567890'), (5, '4713-01-01 BC', 'd', '-2.5'), (10, NULL, 'b', '0.5'), (6, '4713-01-01 BC', 'ab', '1e30'), (-3, '-infinity', 'a', 'Infinity'), (4, '1999-12-31', 'a', '1e30');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "de-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, -3, 'Z', '2.5', '1 day 00:00:00.000001', '[1,5)', '{(,0],[10,)}', 'a fat cat sat on a mat and ate a fat rat', '{}'), (2, 0, 'SS', 'Infinity', '-178000000 years', 'empty', '{[1,2],[2,3)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-1, 'ae')::r5ct, ROW(4, 'å')::r5ct, ROW(4, 'I')::r5ct]), (3, 19, ' a', 'Infinity', '1 mon', '[3,)', '{[1,2],[2,3)}', 'The quick brown fox jumps over the lazy dog', '{}'), (4, 7, 'SS', 'NaN', '178000000 years', '(,2]', '{[1,3),[5,7)}', '', ARRAY[ROW(2, 'h')::r5ct, ROW(-2, 'AB')::r5ct, ROW(1, '2')::r5ct]), (5, 4, 'B', '-0.5', '-178000000 years', '[1,5)', '{[1,2],[2,3)}', 'running runners ran; the runner runs', '{}'), (6, 14, 'ch', '1e100', '1 day 00:00:00.000001', NULL, '{[2,4)}', 'running runners ran; the runner runs', '{}'), (7, 8, 'ss', '-1e40', '1 day 00:00:00.000001', '[-3,0]', '{[0,100)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(0, 'Z')::r5ct, ROW(NULL, NULL)::r5ct, ROW(1, NULL)::r5ct]), (8, 10, 'Z', '1e-130', '-1 mon 30 days', '(,)', '{}', 'the and of', ARRAY[ROW(1, '10')::r5ct, ROW(-2, 'I')::r5ct]), (9, 6, 'ch', '1e-130', NULL, '(,)', '{}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(4, 'B')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_b ON r5a (b COLLATE "C");

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 28;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (14, 'a', '2025-01-01'), (11, 'b', 'infinity'), (12, 'c', '2024-02-29'), (-3, 'd', '2024-02-29'), (16, 'a', NULL), (6, 'b', NULL), (3, 'c', '2024-02-29'), (8, 'd', '2025-01-01'), (-2, 'a', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (30, 'x0', NULL), (23, 'x1', NULL), (25, 'x2', NULL), (27, 'x3', NULL), (32, 'x4', NULL);

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob1', 1, '{}', NULL), (2, 'u2@x.org', '2', 0, NULL, '[1, {"a": null}]'), (3, 'u3@x.org', 'Éva3', 3, '{a,b}', NULL), (4, 'u4@x.org', 'zoë4', 4, NULL, '[1, {"a": null}]'), (5, 'u5@x.org', 'ann5', 5, NULL, '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', '6', 6, '{a}', NULL), (7, 'u7@x.org', 'ann7', 7, '{}', '{}'), (8, 'u8@x.org', 'bob8', 8, NULL, '{"plan": "pro", "seats": 3}'), (9, 'u9@x.org', 'zoë9', 9, '{a,b}', '{"seats": 1.50}'), (10, 'u10@x.org', 'dee10', 0, '{a,b}', '[1, {"a": null}]'), (11, 'u11@x.org', '11', 11, '{b,NULL}', '{"seats": 1.50}'), (12, 'u12@x.org', 'dee12', 12, '{a,b}', NULL);

INSERT INTO r6o (uid, amount, status, created, data) VALUES (11, 0, 'new', '2025-02-23', '{"items": [{"sku": "a", "qty": 2}]}'), (7, 9.99, 'void', '2025-02-28', '{"gift": true}'), (3, 12345678.90, 'paid', '2025-02-20', '{"gift": true}'), (4, NULL, 'paid', '2025-02-04', '{"items": []}'), (12, 0, 'void', '2025-02-26', '{"items": []}'), (5, 0, 'new', '2025-02-09', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 0, 'new', '2025-02-02', '{"items": [{"sku": "a", "qty": 2}]}'), (9, 100, 'new', '2025-02-11', '{"items": [{"sku": "a", "qty": 2}]}'), (2, -1.50, 'void', '2025-02-07', '{"gift": true}'), (5, 12345678.90, 'paid', '2025-02-20', '{"items": []}'), (10, 12345678.90, 'new', '2025-02-02', '{"gift": true}'), (7, 12345678.90, 'new', '2025-02-04', '{"items": []}'), (11, NULL, 'new', '2025-02-05', NULL);

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

INSERT INTO r7k VALUES (1, 1, 184, 'n%5', -12.125, -0, '2024-02-29 12:00:00+00'), (2, 6, 119, 'Note 3', 0, 1e300, NULL), (3, 1, -22, 'n%5', 1000, 0.5, '2024-02-29 12:00:00+00'), (4, 1, 83, 'é', -12.125, -0, '2025-03-09 06:59:59+00'), (5, 9, 181, '', NULL, 0.5, '2024-02-29 12:00:00+00'), (6, 1, 43, 'note 2', 0, 'NaN', '2025-11-02 05:30:00+00'), (7, 2, NULL, 'note 2', NULL, -0, '2024-02-29 12:00:00+00'), (8, 8, 103, '', NULL, 0.5, NULL), (9, 1, 149, 'Note 3', 3.333, 2.25, NULL), (10, 1, -38, 'NOTE 3', -12.125, 0.5, '2025-03-09 06:59:59+00'), (11, 1, NULL, 'note 2', 0.5, NULL, '2025-11-02 05:30:00+00'), (12, 1, 2, 'NOTE 3', 0, NULL, '2025-03-09 06:59:59+00'), (13, 1, 120, 'note 2', NULL, NULL, '2025-11-02 05:30:00+00'), (14, 6, 63, 'note 3', 0.5, 'Infinity', NULL), (15, 1, NULL, 'note_4', -12.125, 'NaN', '2025-11-02 05:30:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12);

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

INSERT INTO r7t VALUES (1, 'é', 'é'), (2, 'ß', 'ß'), (3, 'ss', 'ss'), (4, 'ss', 'ss'), (5, 'b', 'b'), (6, 'a', 'a'), (7, '', '');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT ALL ON r7acct TO r7_bob;

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

INSERT INTO r8a VALUES (1, 4, 2, 3.75, '', '2025-01-01'::timestamp + interval '50 hours'), (2, 3, 0, -2.25, NULL, '2025-01-01'::timestamp + interval '12 hours'), (3, 1, 11, 10, '', '2025-01-01'::timestamp + interval '20 hours'), (4, 3, -2, NULL, 'ab', '2025-01-01'::timestamp + interval '38 hours'), (5, 2, 12, 0, 'a', '2025-01-01'::timestamp + interval '44 hours'), (6, 2, 6, 1.50, 'a', '2025-01-01'::timestamp + interval '8 hours'), (7, 4, -4, 10, 'z', '2025-01-01'::timestamp + interval '51 hours'), (8, 2, -3, 0, 'a', '2025-01-01'::timestamp + interval '100 hours'), (9, 1, 15, -2.25, 'z', '2025-01-01'::timestamp + interval '177 hours'), (10, 4, 0, 3.75, 'B', '2025-01-01'::timestamp + interval '31 hours'), (11, 1, 5, -2.25, NULL, '2025-01-01'::timestamp + interval '100 hours'), (12, 1, NULL, NULL, '', '2025-01-01'::timestamp + interval '94 hours'), (13, 3, 9, 1.50, 'B', '2025-01-01'::timestamp + interval '51 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 3, 8), (2, 6, 5), (4, 5, 6), (4, 5, 1), (6, 5, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, '{[-2,3), [5, 7)}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (2, NULL, NULL, NULL, '(1.25,1.5)'), (3, 'empty', '{}', NULL, '(1.25,1.5)'), (4, '[-3,3)', '{(,-3)}', 'empty', '(,0]'), (5, 'empty', '{(,3)}', '[2025-01-01,2025-02-01)', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'foxes and dogs'), (2, ''), (3, 'dog-eat-dog world'), (4, '');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '"scalar"'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2583) i;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2094) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3787) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

SELECT int4range(NULL, 3) @> int4range(-2, 10, '(]'), int4range(NULL, 3) + int4range(-2, 10, '(]'), range_merge(int4range(NULL, 3), int4range(-2, 10, '(]')), lower(int4range(NULL, 3)), upper_inc(int4range(-2, 10, '(]')), isempty(int4range(NULL, 3) * int4range(-2, 10, '(]')), lower_inf(int4range(NULL, 3));

CREATE TABLE r8ex39881 (id int, room int, during tsrange, EXCLUDE USING gist (during WITH &&));

INSERT INTO r8ex39881 VALUES (1, 1, '[2025-01-01 10:00, 2025-01-01 11:00)');

INSERT INTO r8ex39881 VALUES (2, 1, '[2025-01-01 11:00, 2025-01-01 12:00)');

INSERT INTO r8ex39881 VALUES (3, 2, 'empty'), (4, 2, 'empty');

DROP TABLE r8ex39881;

SET plan_cache_mode = force_generic_plan;

PREPARE r9p52338 (int, int) AS UPDATE r9s SET v = v + $2 WHERE id = $1 RETURNING id, g, v;

EXPLAIN (COSTS OFF) EXECUTE r9p52338(2147483647, 2);

EXECUTE r9p52338(2147483647, 2);

EXECUTE r9p52338(2147483647, 1);

EXECUTE r9p52338(0, 1);

EXPLAIN (COSTS OFF) EXECUTE r9p52338(1499, -1);

EXECUTE r9p52338(1499, -1);

EXECUTE r9p52338(2999, 2);

EXPLAIN (COSTS OFF) EXECUTE r9p52338(2, 7);

EXECUTE r9p52338(2, 7);

SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9p52338';

DEALLOCATE r9p52338;

RESET plan_cache_mode;

-- statement
SELECT sum(v) OVER (w RANGE 1 PRECEDING) FROM r8a WINDOW w AS (ORDER BY g ROWS 1 PRECEDING);
