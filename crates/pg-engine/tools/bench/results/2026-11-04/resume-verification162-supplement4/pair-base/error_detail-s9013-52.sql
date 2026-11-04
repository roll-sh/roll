-- class: error_detail
-- ordered: true
-- note: 42P01
-- engine: ERROR 42P01: invalid reference to FROM-clause entry for table "s2"
-- engine: DETAIL: There is an entry for table "s2", but it cannot be referenced from this part of the query.
-- engine: HINT: To reference that table, you must mark this subquery with LATERAL.
-- engine: POSITION: 372
-- reference: ERROR 42P01: invalid reference to FROM-clause entry for table "s2"
-- reference: DETAIL: There is an entry for table "s2", but it cannot be referenced from this part of the query.
-- reference: POSITION: 372

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int4 DEFAULT 7, b d3text);

INSERT INTO t0 (id, a, b) VALUES (39, NULL, NULL);

CREATE INDEX t0_i372 ON t0 (id, a);

CREATE INDEX t0_i718 ON t0 ((a + 1));

CREATE INDEX t0_i905 ON t0 (id);

CREATE TABLE t1 (id int4, a text, b int8) PARTITION BY RANGE (a);

CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES FROM (MINVALUE) TO ('b');

CREATE TABLE t1_pd PARTITION OF t1 DEFAULT;

INSERT INTO t1 (id, a, b) VALUES (40, NULL, NULL), (18, 'aa', 6), (24, 'ab', 6);

CREATE UNIQUE INDEX t1_i193 ON t1 (id) NULLS NOT DISTINCT;

CREATE INDEX t1_i319 ON t1 USING hash (a);

CREATE INDEX t1_i773 ON t1 USING hash (id);

CREATE TABLE t2 (a int4, b int4, g int GENERATED ALWAYS AS (coalesce(a, 0) * 2 + 1) VIRTUAL);

CREATE TABLE t2_c (extra int) INHERITS (t2);

INSERT INTO t2_c (a, b, extra) VALUES (NULL, 5, 1);

INSERT INTO t2 (a, b) VALUES (8, 7), (5, 3), (2, 8), (5, -1), (3, NULL), (6, 9), (NULL, NULL), (9, 8), (NULL, 7), (NULL, NULL), (NULL, NULL), (NULL, NULL), (2, 1), (2, -2), (NULL, 2), (7, 8), (0, 6), (2, 0), (9, NULL), (NULL, NULL);

CREATE INDEX t2_i984 ON t2 (g, a);

CREATE INDEX t2_i69 ON t2 USING hash (g);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE r int := 0;
BEGIN
  IF CASE WHEN true THEN true ELSE false END IS DISTINCT FROM true THEN
    r := 7;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(extract(day from NULL::date)::int, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT CASE WHEN false THEN ((NULL::jsonb || '1'::jsonb)::jsonb ->> 'a')::int ELSE p END $$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT a FROM t2 WHERE a <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT a FROM t2 WHERE a IS NULL;
  END IF;
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

INSERT INTO tj4 VALUES (1, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', NULL, '4713-01-01 BC', 'infinity', '-1e-20', '1024', 1), (2, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '1999-12-31 23:59:59.999999+00', '2025-12-31 23:59:59.999999', '2025-01-01', '-12:00', '2.5', '2', 0), (3, NULL, NULL, '2025-11-02 01:30:00-05', '2025-03-09 02:30:00', '2024-02-29', '-infinity', '2.5', '1024', 6), (4, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', '2024-02-29 00:00:00', '-infinity', NULL, '-0.0', '1024', 3), (5, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2025-11-02 01:30:00', '4713-01-01 BC', '23:59:59.999999', '0.000001', '-1.25', 0);

INSERT INTO tp4 VALUES (-1, '2025-03-09', 'c', '1e30'), (24, '1999-12-31', 'b', '123.45'), (-2, '2000-03-01', NULL, '2.5'), (4, '1999-12-31', 'b', '-7'), (17, '2000-03-01', 'c', '-0.0'), (12, '2025-03-09', 'd', '0'), (12, 'infinity', NULL, '-Infinity'), (13, '2025-01-01', 'ab', '0.000001'), (9, '2025-01-01', 'c', NULL), (1, '2000-03-01', 'b', 'Infinity'), (14, '4713-01-01 BC', 'ab', '-1e-20');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "POSIX", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, NULL, 'e', '-1e40', '1 year -1 sec', '[3,)', '{empty}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(0, 'Ab')::r5ct, ROW(0, 'E')::r5ct]), (2, 19, 'B', NULL, '720:00:00', NULL, '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', '{}'), (3, 7, 'AB', 'Infinity', '2147483647 days', '[1,5)', '{[0,100)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(3, 'ß')::r5ct, ROW(2, 'e')::r5ct, ROW(2, 'E')::r5ct]), (4, 18, 'Ab', '0.0000000000000000000000000001', '1 year -1 sec', '[5,6)', '{[0,100)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(1, 'ı')::r5ct, ROW(4, 'b')::r5ct, ROW(2, 'h')::r5ct]), (5, 12, 'ß', 'Infinity', '30 days', '[-3,0]', '{(,0],[10,)}', 'a fat cat sat on a mat and ate a fat rat', ARRAY[ROW(3, 'ss')::r5ct]), (6, 4, 'Z', '-1e40', '-178000000 years', NULL, '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(-1, 'a')::r5ct, ROW(NULL, 'A')::r5ct, ROW(1, 'é')::r5ct]), (7, 16, 'i', '-0.5', '-178000000 years', NULL, '{[1,2],[2,3)}', 'the and of', ARRAY[ROW(-1, 'ı')::r5ct]), (8, -3, 'a', '7', '30 days', '(,2]', '{(,0],[10,)}', 'running runners ran; the runner runs', NULL), (9, 6, 'ö', '0.0000000000000000000000000001', '-178000000 years', '[0,10)', '{[2,4)}', 'Fat cats! FAT rats? fat-free', NULL), (10, 7, 'ab', '-1e40', '1 mon', '[0,10)', '{}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(2, 'ss')::r5ct, ROW(5, 'é')::r5ct]), (11, 19, NULL, '-0.5', '30 days', '[0,10)', '{[0,100)}', NULL, '{}');

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 0 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 14 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (-5, 'a', '2024-02-29'), (10, 'b', 'infinity'), (-3, 'c', '2024-02-29'), (13, 'd', 'infinity'), (9, 'a', '2025-01-01'), (-2, 'b', NULL) ON CONFLICT DO NOTHING;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', '1', 0, '{b,NULL}', '[1, {"a": null}]'), (2, 'u2@x.org', 'Cid2', 0, '{}', '[1, {"a": null}]'), (3, 'u3@x.org', '3', 3, NULL, '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', 'bob4', 0, '{a}', '{"plan": "free", "tags": [1, 2]}'), (5, 'u5@x.org', 'Cid5', 0, '{}', '{"plan": "free", "tags": [1, 2]}'), (6, 'u6@x.org', 'dee6', 0, '{a,b}', NULL), (7, 'u7@x.org', 'bob 7', 0, '{a,b}', '{}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 100, 'new', '2025-02-28', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 100, 'void', '2025-02-12', '{"items": []}'), (3, -1.50, 'new', '2025-02-09', '{"items": []}'), (6, 100, 'new', '2025-02-07', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 9.99, 'new', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 100, 'new', '2025-02-15', '{"items": []}'), (7, 9.99, 'new', '2025-02-15', '{"items": [{"sku": "a", "qty": 2}]}'), (7, 12345678.90, 'paid', '2025-02-24', NULL), (5, -1.50, 'void', '2025-02-03', '{"items": []}'), (7, 100, 'void', '2025-02-01', '{"items": []}'), (3, -1.50, 'paid', '2025-02-21', NULL), (4, 9.99, 'paid', '2025-02-09', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 12345678.90, 'new', '2025-02-01', '{"items": []}');

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

INSERT INTO r7k VALUES (1, 1, 109, 'Note 3', 0, -0, NULL), (2, 1, 63, 'NOTE 3', -12.125, 1e300, '2025-03-09 06:59:59+00'), (3, 7, -6, 'note_4', -12.125, NULL, NULL), (4, 2, 126, 'é', NULL, 0.5, '2025-11-02 05:30:00+00'), (5, 1, -46, 'NOTE 3', -12.125, 2.25, '2025-03-09 06:59:59+00'), (6, 1, 158, 'Note 3', 0.5, 1e300, '2025-11-02 05:30:00+00'), (7, 1, 155, 'note 2', 3.333, 2.25, '2025-03-09 07:00:00+00'), (8, 3, NULL, 'Note 3', 0.5, 0.5, '2025-11-02 05:30:00+00'), (9, 1, 190, 'note 3', NULL, 1e300, NULL), (10, 1, -45, 'n%5', -12.125, 'NaN', '2024-02-29 12:00:00+00'), (11, 7, 121, 'NOTE 3', 3.333, -0, '2025-03-09 06:59:59+00'), (12, 1, -17, 'note 2', 1000, NULL, '2025-11-02 05:30:00+00'), (13, 1, 195, 'Note 3', 0, -0, '2025-11-02 05:30:00+00');

CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region);

CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu');

CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us');

CREATE TABLE r7p_def PARTITION OF r7p DEFAULT;

INSERT INTO r7p VALUES (1, 'eu', 7), (2, 'eu', 14), (3, 'us', 21), (4, 'eu', 5), (5, 'eu', 12), (6, 'us', 19), (7, 'eu', 3), (8, 'eu', 10);

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

INSERT INTO r7t VALUES (1, 'z', 'z'), (2, '10', '10'), (3, '', ''), (4, 'E', 'E'), (5, 'A', 'A'), (6, 'ä', 'ä'), (7, '9', '9'), (8, 'ß', 'ß'), (9, 'ä', 'ä');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT, DELETE ON r7acct TO r7_bob;

GRANT SELECT (id, grp) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, 0, 10, 'B', '2025-01-01'::timestamp + interval '0 hours'), (2, 2, 19, -2.25, 'B', '2025-01-01'::timestamp + interval '98 hours'), (3, 3, 18, NULL, '', '2025-01-01'::timestamp + interval '183 hours'), (4, 3, 14, -2.25, 'a', '2025-01-01'::timestamp + interval '30 hours'), (5, 3, NULL, -2.25, 'b', '2025-01-01'::timestamp + interval '32 hours'), (6, 2, 16, NULL, 'B', '2025-01-01'::timestamp + interval '3 hours'), (7, 1, NULL, 3.75, 'a', '2025-01-01'::timestamp + interval '24 hours'), (8, 3, -4, 1.50, 'z', '2025-01-01'::timestamp + interval '39 hours'), (9, 3, -3, NULL, 'b', '2025-01-01'::timestamp + interval '28 hours'), (10, 3, 19, 10, NULL, '2025-01-01'::timestamp + interval '132 hours'), (11, 3, -4, 3.75, 'z', '2025-01-01'::timestamp + interval '183 hours'), (12, 1, 4, -2.25, NULL, '2025-01-01'::timestamp + interval '0 hours'), (13, 4, 1, 0, '', '2025-01-01'::timestamp + interval '27 hours'), (14, 1, NULL, 1.50, '', '2025-01-01'::timestamp + interval '25 hours'), (15, 3, 20, 0, 'ab', '2025-01-01'::timestamp + interval '87 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 4, 6), (1, 4, 5), (2, 7, 6), (2, 5, 2), (3, 4, 4), (3, 4, 3), (4, 4, 2), (4, 8, 6), (5, 8, 9), (6, 3, 1), (6, 4, 5), (7, 3, 7), (7, 4, 3), (8, 7, 8), (8, 1, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '(2,5]', '{(,2)}', NULL, NULL), (2, '(-3,-1]', NULL, '(2024-12-31,2025-01-05]', '(,0]'), (3, 'empty', '{[0,0), [2, 4)}', '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (4, '[1,)', '{(,1)}', '[2025-03-01,infinity)', NULL), (5, NULL, '{(,0)}', 'empty', NULL), (6, '[2,4)', '{}', 'empty', '[1.5,2.5)');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'quick quick slow'), (2, 'quick quick slow'), (3, 'a rat ran'), (4, 'foxes and dogs'), (5, 'jumps over the lazy dog'), (6, 'über café naïve'), (7, 'foxes and dogs'), (8, 'running rats');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '"scalar"'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (6, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3617) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2735) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3189) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

SELECT lo_from_bytea(887303, '\x00ff00ff')::int8 - 887303;

SELECT lo_get(887303), lo_get(887303, 3, 2);

BEGIN;

SELECT lo_open(887303, x'20000'::int);

SELECT loread(0, 3);

SELECT lo_lseek(0, -1, 3);

SELECT lowrite(0, '\x5a5a');

SELECT lo_tell(0), lo_lseek64(0, 0, 2);

SELECT lo_truncate(0, 0);

SELECT loread(0, 100);

SELECT lo_close(0);

ROLLBACK;

SELECT lo_get(887303);

SELECT lo_put(887303, 17, '\x7171');

SELECT lo_get(887303), length(lo_get(887303));

SELECT oid::int8 - 887303, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = 887303;

SELECT lo_create(887303);

SELECT lo_unlink(887303);

SELECT lo_get(887303);

CREATE TABLE r8t66245 (id int PRIMARY KEY, v int, s text);

INSERT INTO r8t66245 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL);

CREATE FUNCTION r8tf66245() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('t66245', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION r8tf66245_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('t66245', r);
  RETURN NULL;
END $$;

CREATE TRIGGER i_td AFTER DELETE ON r8t66245 REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r8tf66245_tt();

CREATE TRIGGER f_tt AFTER UPDATE ON r8t66245 REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r8tf66245_tt();

CREATE TRIGGER b_air AFTER INSERT OR DELETE ON r8t66245 FOR EACH ROW EXECUTE FUNCTION r8tf66245();

INSERT INTO r8t66245 VALUES (10, -2, 'n') RETURNING *;

DELETE FROM r8log WHERE tg = 't66245';

INSERT INTO r8t66245 VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + r8t66245.v RETURNING *;

DELETE FROM r8log WHERE tg = 't66245';

DELETE FROM r8t66245 WHERE id = 3 RETURNING *;

DELETE FROM r8log WHERE tg = 't66245';

UPDATE r8t66245 SET v = coalesce(v, 0) - 1 RETURNING id, v;

DELETE FROM r8log WHERE tg = 't66245';

INSERT INTO r8t66245 SELECT g, g - 22, 'x' FROM generate_series(20, 23) g;

DELETE FROM r8log WHERE tg = 't66245';

BEGIN;

UPDATE r8t66245 SET s = 'deferred' WHERE id = 2;

SET CONSTRAINTS ALL IMMEDIATE;

COMMIT;

DELETE FROM r8log WHERE tg = 't66245';

DROP TABLE r8t66245;

DROP FUNCTION r8tf66245, r8tf66245_tt;

-- statement
SELECT abs(11::int8) AS c0, initcap(initcap(trim(t1.a))) AS c1, lpad(repeat(t1.a, 2), 3, '*') AS c2, (array_length(('{1,2}'::int4[] || '{1,2}'::int4[]), 1), 'foo') <= (array_position('{2,NULL}'::int4[], ascii(t1.a)), array_to_string('{ab}'::text[], ',', '?')) AS c3 FROM ((t1 t1 JOIN unnest('{1,2}'::int4[]) WITH ORDINALITY s2(u, o) ON t1.id = s2.u) RIGHT JOIN (VALUES ((s2.u / s2.u)), ((-2)), (10)) s3(v0) ON s2.u = s3.v0 AND CASE WHEN true THEN 3::numeric ELSE 'NaN'::numeric END = COALESCE('NaN'::numeric, 10.00::numeric)) ORDER BY 1 DESC, 2 DESC, 3, 4 NULLS FIRST LIMIT 4;
