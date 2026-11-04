-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a8738  (rows=15)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a8738  (rows=992)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a numeric(10,2), b jsonb, c timestamp, d timestamptz, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED);

INSERT INTO t0 (id, a, b, c, d) VALUES (12, NULL, NULL, '2024-01-07 06:00:00', '2024-01-05 23:00:00+00'), (28, NULL, NULL, NULL, NULL), (7, NULL, '{"a": "1"}', '2024-01-09 00:30:00', NULL), (18, NULL, NULL, '2024-01-07 12:00:00', NULL), (10, NULL, NULL, '2024-01-04 06:00:00', '2024-01-07 06:00:00+00'), (11, 8.05, 'null', '2024-01-08 06:30:00', NULL), (16, NULL, NULL, NULL, NULL), (53, NULL, NULL, '2024-01-08 12:30:00', '2024-01-07 12:00:00+00'), (20, NULL, NULL, NULL, NULL), (31, NULL, NULL, NULL, NULL), (33, NULL, NULL, NULL, NULL), (1, NULL, NULL, NULL, NULL);

CREATE INDEX t0_i474 ON t0 ((id + 1));

CREATE INDEX t0_i879 ON t0 (b, a);

CREATE INDEX t0_i513 ON t0 (b);

CREATE TABLE t1 (id int4 PRIMARY KEY, a json, b bool, c int2, d timestamp, e jsonb);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (16, '[1, 2, 3]', NULL, NULL, '2024-01-03 00:30:00', NULL), (39, NULL, true, NULL, NULL, '{"a": 1, "b": "x"}'), (30, NULL, false, NULL, NULL, '{"a": 1, "b": "x"}'), (6, NULL, NULL, NULL, NULL, NULL), (22, NULL, NULL, NULL, NULL, NULL), (44, '{"a": {"b": 2}}', true, NULL, NULL, '{"a": "1"}'), (13, NULL, NULL, 6, NULL, NULL), (17, 'null', true, NULL, NULL, NULL), (12, NULL, NULL, NULL, '2024-01-05 06:00:00', NULL), (25, NULL, NULL, NULL, NULL, NULL), (14, NULL, NULL, 6, '2024-01-01 00:30:00', NULL), (19, NULL, NULL, NULL, NULL, NULL), (28, '{"a": {"b": 2}}', NULL, NULL, NULL, NULL), (29, NULL, NULL, 6, '2024-01-06 06:00:00', '{"a": 3}'), (27, NULL, NULL, NULL, NULL, 'null'), (11, NULL, true, NULL, NULL, '{"a": "1"}'), (47, '[1, 2, 3]', NULL, NULL, NULL, '{"a": {"b": 2}}'), (38, NULL, NULL, NULL, NULL, NULL), (35, '{"a": {"b": 2}}', NULL, 2, '2024-01-03 23:30:00', NULL), (33, 'null', NULL, 6, '2024-01-04 06:00:00', NULL), (3, NULL, NULL, NULL, '2024-01-03 06:00:00', NULL), (37, NULL, NULL, NULL, '2024-01-02 23:30:00', NULL), (34, NULL, NULL, NULL, NULL, NULL), (18, NULL, false, 0, NULL, 'null'), (20, NULL, NULL, NULL, NULL, '[1, 2, 3]'), (15, NULL, NULL, NULL, NULL, '1'), (59, NULL, NULL, NULL, '2024-01-06 06:00:00', NULL), (46, '{"a": 1, "b": "x"}', false, 3, NULL, NULL), (24, NULL, NULL, NULL, NULL, NULL), (48, '[]', NULL, NULL, NULL, '{"a": 1, "b": "x"}');

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 1 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('foo', 3)::ct3, ARRAY[ROW('a%', 2)::ct3, ROW('', 1)::ct3], 3), (2, ROW(NULL, -1)::ct3, NULL, 3), (3, ROW('foo', 1)::ct3, NULL, 3), (4, ROW('b', -1)::ct3, ARRAY[ROW('abc', 2)::ct3, ROW('a_b', NULL)::ct3], NULL), (5, ROW('', 0)::ct3, ARRAY[ROW('b', 0)::ct3, ROW('B', 0)::ct3], NULL), (6, ROW('Ab', 1)::ct3, ARRAY[ROW('Ä', -1)::ct3, ROW('b', NULL)::ct3], NULL), (7, ROW('x ', 2)::ct3, '{}', NULL), (8, NULL, ARRAY[ROW('ß', 0)::ct3, ROW(NULL, 0)::ct3], 1), (9, ROW('z', -1)::ct3, ARRAY[ROW(NULL, -1)::ct3, ROW('9', 3)::ct3], NULL), (10, NULL, ARRAY[ROW('10', NULL)::ct3, ROW('a_b', 2)::ct3], NULL), (11, ROW('ba', -1)::ct3, ARRAY[ROW(NULL, NULL)::ct3, ROW(NULL, 1)::ct3], 2), (12, ROW('b', 3)::ct3, ARRAY[ROW(' x', 1)::ct3, ROW('Ä', 1)::ct3], NULL);

CREATE INDEX tc3_cd ON tc3 (c DESC NULLS LAST, n);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql STABLE AS $$ SELECT CASE WHEN false THEN sign(NULLIF((-4), p))::int ELSE p END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE r int := 0;
BEGIN
  IF q < q COLLATE "POSIX" THEN
    r := p;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce((COALESCE('1'::jsonb, '{"a": null}'::jsonb)::jsonb ->> 'a')::int, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT json_build_array((7 & (-2147483648)), (SELECT CASE WHEN t3.b THEN 'abc' ELSE 'ab' END FROM t1 t3 WHERE '{"a": 1, "b": "x"}'::jsonb > ('{"a": 2.50}'::jsonb - 'a') ORDER BY 1 LIMIT 1)) AS c0, array_agg(t2.g ORDER BY t2.g NULLS FIRST) AS c1, regexp_replace(min(' x'), '[ab]', 'y', 'g') AS c2, min((t2.c + ('2000-04-19 17:59:00'::timestamp - t2.c))) AS c3 FROM t0 t2 GROUP BY t2.b;

CREATE VIEW v1 AS SELECT '2000-01-02 16:00:00+00'::timestamptz AS c0, (- (- 2::float8)) AS c1, ('1999-11-07 08:30:00'::timestamp - CASE WHEN true THEN '2025-12-17 19:30:00'::timestamp ELSE '1999-12-16 04:30:00'::timestamp END) AS c2, jsonb_agg(CASE WHEN false THEN jsonb_array_length('"s"'::jsonb) ELSE extract(day from '2024-11-22'::date)::int END ORDER BY CASE WHEN false THEN jsonb_array_length('"s"'::jsonb) ELSE extract(day from '2024-11-22'::date)::int END) AS c3 FROM generate_series(1, 2) s1(g) WHERE false GROUP BY s1.g HAVING bool_and('foo' NOT LIKE '^a');

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}', '2025-03-09 01:30:00-05', '2025-12-31 23:59:59.999999', '1999-12-31', '-infinity', '123.45', '0.125', 10), (2, '{"a": 5, "s": "", "ts": "2024-02-29"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '2025-03-09 02:30:00', '-infinity', NULL, '-2.5', '-0', 7), (3, '[1, 2, {"a": 3}]', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', '2025-10-26 02:30:00', '2025-03-09', NULL, '-7', '2', 11), (4, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', '2025-11-02 01:30:00', '2025-01-01', '-1 mon 2 days', '-0.0', NULL, 6), (5, '{}', '{}', '0044-03-15 12:00:00+00 BC', '2025-11-02 01:30:00', '-infinity', '23:59:59.999999', NULL, '-1.25', 1);

INSERT INTO tp4 VALUES (3, '2000-03-01', 'b', NULL), (24, NULL, NULL, NULL), (-1, '2025-01-01', 'c', '0'), (13, NULL, 'd', 'Infinity'), (-3, '2024-02-29', 'ab', 'Infinity'), (8, '2025-03-09', 'a', 'NaN'), (19, NULL, 'c', '-Infinity');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "tr-x-icu", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) STORED, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 13, 'Ab', '7', 'infinity', '[5,6)', '{empty}', NULL, ARRAY[ROW(2, NULL)::r5ct]), (2, -1, 'E', '1e100', '720:00:00', '(1,5]', NULL, NULL, NULL), (3, 3, '10', '1e-130', '-2147483648 days', 'empty', '{}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(NULL, 'b')::r5ct]), (4, 17, NULL, 'NaN', '-1 mon 30 days', '[3,)', '{[1,3),[5,7)}', 'PostgreSQL full text search: lexemes & weights', NULL), (5, 10, NULL, '-Infinity', '-178000000 years', '[5,6)', '{[0,100)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(5, 'Z')::r5ct, ROW(5, 'ß')::r5ct]), (6, 19, 'A', '123456789012345678901234567890.123456789', '720:00:00', '(,)', '{empty}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(NULL, '')::r5ct, ROW(NULL, '-a')::r5ct, ROW(5, 'ı')::r5ct]), (7, 11, 'ö', '-99999999999999999999999999999999999.999999', '-178000000 years', '[5,6)', '{(,0],[10,)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(-2, 'I')::r5ct, ROW(2, 'é')::r5ct, ROW(-2, 'z')::r5ct]);

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -1 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 17 WITH CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b);

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (15, 'a', '2024-02-29'), (17, 'b', 'infinity'), (-2, 'c', NULL) ON CONFLICT DO NOTHING;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'bob 1', 1, '{a}', '{}'), (2, 'u2@x.org', 'zoë2', 2, '{a,b}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'dee3', 3, '{a}', '[1, {"a": null}]'), (4, 'u4@x.org', 'Cid4', 4, '{}', '{}'), (5, 'u5@x.org', '5', 0, '{a,b}', '{"plan": "pro", "seats": 3}'), (6, 'u6@x.org', 'dee6', 6, '{a}', '{"seats": 1.50}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (1, 0, 'new', '2025-02-10', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 100, 'new', '2025-02-16', NULL), (5, 9.99, 'void', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 100, 'new', '2025-02-13', NULL), (6, 12345678.90, 'new', '2025-02-16', '{"gift": true}'), (6, 12345678.90, 'new', '2025-02-04', NULL), (4, 100, 'paid', '2025-02-14', '{"items": []}'), (4, NULL, 'paid', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 0, 'void', '2025-02-24', NULL), (5, 12345678.90, 'paid', '2025-02-13', '{"items": []}'), (3, 0, 'paid', '2025-02-24', '{"items": []}'), (6, 12345678.90, 'paid', '2025-02-05', NULL), (3, 12345678.90, 'void', '2025-02-09', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 176, 'NOTE 3', -12.125, 2.25, '2025-03-09 06:59:59+00'), (2, 3, 82, 'Note 3', 3.333, NULL, '2024-02-29 12:00:00+00'), (3, 1, 181, 'note_4', 0, 'NaN', '2024-02-29 12:00:00+00'), (4, 1, 13, '', 1000, -0, NULL), (5, 1, 120, 'note_4', 0, 'Infinity', '2025-03-09 07:00:00+00'), (6, 8, 188, 'note 3', -12.125, 'NaN', NULL), (7, 8, 172, 'NOTE 3', 3.333, 1e300, '2025-11-02 05:30:00+00'), (8, 1, 114, '', 0, NULL, '2025-03-09 06:59:59+00'), (9, 8, NULL, 'é', -12.125, -0, '2025-11-02 05:30:00+00'), (10, 1, 99, 'note 3', 0, 2.25, '2024-02-29 12:00:00+00'), (11, 1, NULL, 'n%5', 1000, NULL, '2025-03-09 06:59:59+00'), (12, 1, 167, 'note_4', NULL, 2.25, NULL), (13, 1, 139, 'é', 0, 'NaN', NULL), (14, 1, -35, 'n%5', 0, NULL, '2025-03-09 06:59:59+00'), (15, 1, -12, 'Note 3', 1000, NULL, '2024-02-29 12:00:00+00'), (16, 1, -11, 'n%5', 1000, 2.25, '2025-03-09 06:59:59+00'), (17, 1, 44, 'note_4', 0, 'Infinity', '2024-02-29 12:00:00+00'), (18, 1, 4, 'note_4', NULL, 'Infinity', '2024-02-29 12:00:00+00'), (19, 2, 150, 'note_4', -12.125, 2.25, '2024-02-29 12:00:00+00'), (20, 1, 5, 'Note 3', NULL, 1e300, '2025-03-09 06:59:59+00'), (21, 2, -32, 'é', 1000, NULL, '2024-02-29 12:00:00+00'), (22, 1, 88, 'note 2', 3.333, 'NaN', '2024-02-29 12:00:00+00');

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

INSERT INTO r7t VALUES (1, 'é', 'é'), (2, 'Ä', 'Ä'), (3, 'ß', 'ß'), (4, '', ''), (5, 'ae', 'ae'), (6, '', '');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 4, 8, 1.50, '', '2025-01-01'::timestamp + interval '48 hours'), (2, 3, -1, 0, '', '2025-01-01'::timestamp + interval '87 hours'), (3, 4, 6, 0, 'a', '2025-01-01'::timestamp + interval '49 hours'), (4, 4, 3, 10, 'B', '2025-01-01'::timestamp + interval '52 hours'), (5, 1, 8, -2.25, 'B', '2025-01-01'::timestamp + interval '108 hours'), (6, 2, 7, 3.75, 'z', '2025-01-01'::timestamp + interval '2 hours'), (7, 1, 18, 3.75, 'z', '2025-01-01'::timestamp + interval '10 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 1, 8), (1, 1, 3), (2, 1, 1), (2, 2, 5), (3, 2, 9), (3, 2, 7), (4, 3, 5), (4, 3, 6), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, NULL, NULL, '[2025-01-01,2025-02-01)', '(,0]'), (2, 'empty', '{(,-1)}', NULL, '[1.5,2.5)'), (3, '[2,)', NULL, '(2024-12-31,2025-01-05]', '[0,0]'), (4, '[1,)', '{}', '[2025-03-01,infinity)', '(,0]'), (5, '[3,)', '{(,3)}', 'empty', '[1.5,2.5)'), (6, '[7,)', NULL, '[2025-01-01,2025-02-01)', '[0,0]'), (7, '[2,6)', '{(,2)}', '(2024-12-31,2025-01-05]', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, 'running rats'), (3, 'brown''s fox''s den'), (4, ''), (5, 'quick quick slow'), (6, 'the quick brown fox'), (7, 'jumps over the lazy dog'), (8, 'über café naïve'), (9, 'über café naïve');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2582) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2765) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2024) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

\s2 BEGIN;

\s2 UPDATE r7k SET v = coalesce(v, 0) + 10, grp = grp + 0 WHERE id = 8;

\s3 BEGIN ISOLATION LEVEL REPEATABLE READ;

\s3 SELECT count(*) FROM r7k;

\s3& SELECT id, v FROM r7k WHERE id = 8 FOR NO KEY UPDATE;

\s2 ROLLBACK;

\s3.;

\s3 COMMIT;

\s2 ROLLBACK;

\s3 ROLLBACK;

CREATE FUNCTION r8f57709(a int, b int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text := ''; i int; j int;
BEGIN
  <<outer>> FOR i IN REVERSE a..b BY 1 LOOP
    CONTINUE WHEN i % 3 = 0;
    j := 0;
    <<inner>> LOOP
      j := j + 1;
      EXIT outer WHEN length(r) > 40;
      EXIT inner WHEN j >= i;
      CONTINUE outer WHEN j = 4;
      r := r || i || ':' || j || ' ';
    END LOOP inner;
    WHILE j > 0 AND i < 0 LOOP j := j - 1; r := r || 'w'; END LOOP;
  END LOOP outer;
  RETURN r;
END $$;

SELECT r8f57709(9, 1);

SELECT r8f57709(3, -1);

SELECT r8f57709(NULL, 1);

DROP FUNCTION r8f57709;

CREATE TABLE r9a915 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a915 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 4549, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 4549) i;

UPDATE r9a915 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a915 (g, s, arr);

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a915 WHERE nul IS NULL$$);

SELECT * FROM r9est($$SELECT * FROM r9a915 WHERE u < 100$$);

DROP TABLE r9a915;

CREATE TABLE r9a8738 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a8738 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 2974, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 2974) i;

SET default_statistics_target = 10;

ALTER TABLE r9a8738 ALTER COLUMN g SET (n_distinct = -0.5);

ANALYZE r9a8738;

RESET default_statistics_target;

SELECT * FROM r9est($$SELECT * FROM r9a8738 WHERE g = 1 AND u % 10 = 1$$);

SELECT * FROM r9est($$SELECT DISTINCT g, u % 10 FROM r9a8738$$);

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a8738 WHERE arr @> '{2}'$$);
