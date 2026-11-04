-- class: wrong_rows
-- ordered: false
-- note: 2 vs 2 rows
-- engine: r9est:25
-- engine: Seq Scan on r9a91616  (rows=19)
-- engine:   Filter: (arr @> '{2}'::integer[])
-- engine: -- SELECT 2
-- reference: r9est:25
-- reference: Seq Scan on r9a91616  (rows=1257)
-- reference:   Filter: (arr @> '{2}'::integer[])
-- reference: -- SELECT 2

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4, a text, b text[], c timestamptz, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) STORED) PARTITION BY RANGE (a);

CREATE TABLE t0_p0 PARTITION OF t0 FOR VALUES FROM (MINVALUE) TO ('b');

CREATE TABLE t0_pd PARTITION OF t0 DEFAULT;

CREATE TABLE t1 (a timestamptz, b date, c interval, d timestamp);

INSERT INTO t1 (a, b, c, d) VALUES ('2024-01-06 23:00:00+00', '2024-01-14', '1 day', NULL), ('2024-01-09 00:00:00+00', NULL, NULL, NULL), (NULL, NULL, '-1 day', NULL), (NULL, '2024-01-26', '30 days', NULL), (NULL, NULL, '-1 day', '2024-01-02 00:30:00'), ('2024-01-02 23:00:00+00', '2024-03-05', NULL, NULL), ('2024-01-09 12:00:00+00', NULL, NULL, NULL), (NULL, NULL, '1 mon', NULL), ('2024-01-08 23:00:00+00', NULL, NULL, NULL), (NULL, NULL, NULL, '2024-01-07 06:00:00'), (NULL, '2024-03-01', NULL, NULL), ('2024-01-04 00:00:00+00', NULL, NULL, NULL), (NULL, '2024-03-18', '-1 day', NULL), ('2024-01-09 12:00:00+00', NULL, NULL, NULL), ('2024-01-08 23:00:00+00', '2024-03-20', NULL, NULL), (NULL, NULL, NULL, NULL), (NULL, NULL, NULL, NULL), (NULL, NULL, NULL, '2024-01-07 06:30:00'), (NULL, NULL, '2 hours', '2024-01-06 00:30:00'), (NULL, NULL, NULL, '2024-01-06 06:30:00');

CREATE INDEX t1_i105 ON t1 USING hash (c);

CREATE INDEX t1_i134 ON t1 (d);

CREATE TABLE t2 (id int4 PRIMARY KEY, a timestamp, b date);

INSERT INTO t2 (id, a, b) VALUES (21, '2024-01-03 12:00:00', NULL), (35, '2024-01-08 23:30:00', '2024-02-08'), (40, '2024-01-03 00:00:00', '2024-01-22');

CREATE INDEX t2_i625 ON t2 USING hash (b);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 0 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t2 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t2 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "und-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('', -1)::ct3, ARRAY[ROW('a,b', 2)::ct3, ROW('', 1)::ct3], 0), (2, ROW(NULL, -1)::ct3, '{}', NULL), (3, ROW('abc', 0)::ct3, NULL, NULL), (4, ROW('a', 1)::ct3, NULL, NULL), (5, ROW('aa', 1)::ct3, ARRAY[ROW('', NULL)::ct3, ROW('ß', 2)::ct3], 0), (6, ROW(NULL, -1)::ct3, ARRAY[ROW('9', 2)::ct3, ROW('Z', 1)::ct3], NULL), (7, ROW(NULL, NULL)::ct3, NULL, NULL), (8, ROW('a%', 0)::ct3, '{}', 1), (9, ROW(' x', 0)::ct3, ARRAY[ROW('a,b', 1)::ct3, ROW('é', 1)::ct3], 0);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

CREATE TYPE ctr3 AS RANGE (subtype = ct3);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql  AS $$ SELECT CASE WHEN (p)::int8 IN (SELECT ((GREATEST(t1.id, t1.id))::int8 * ascii(regexp_replace(q, '[ab]', 'y', 'g'))) FROM (t2 t1 JOIN t1 t2 ON t1.b = t2.b) WHERE true) THEN CASE WHEN '{}'::text[] && '{b,NULL}'::text[] THEN ((-2147483648) & p) ELSE array_ndims((ARRAY[[1,2,3],[4,5,6]])[2:2]) END ELSE (p + ascii('ab')) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql STABLE AS $$
BEGIN
  CASE WHEN p IS NULL THEN RETURN NULL;
  WHEN true THEN RETURN (p / p);
  ELSE RETURN (extract(day from 'infinity'::date)::int / 1);
  END CASE;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int, y text) LANGUAGE sql STABLE AS $$ SELECT id, a FROM t0 WHERE id <= lim $$;

CREATE VIEW v0 AS SELECT avg(((2.50::numeric * 99.99::numeric))::int4) AS c0, NULLIF(('1999-10-22 06:00:00'::timestamp)::date, CASE WHEN true THEN '1999-05-04'::date ELSE '2024-04-01'::date END) AS c1 FROM srf0((4::int8)::int4) s1 GROUP BY s1.x HAVING (int4range(s1.x, bit_and(s1.x) FILTER (WHERE 'Z' < s1.y COLLATE "POSIX"), '[]') @> f1(0, 'é'));

CREATE VIEW v1 AS SELECT date_trunc('hour', COALESCE(t1.a, (t2.a AT TIME ZONE 'UTC'))) AS c0, 'x ' AS c1 FROM (t1 t1 LEFT JOIN t2 t2 ON t1.b = t2.b AND (starts_with('a%', '9') OR NULL::numeric <> SOME (WITH w3 AS (SELECT mod((abs(1::numeric) - NULLIF(t4.id, t4.id)), -7) AS c0, JSON_QUERY((('1'::jsonb -> 'a') - 'a'), 'strict $.b')::text AS c1 FROM t2 t4), w5 AS MATERIALIZED (SELECT 8::int2 AS c0, array_length((('{}'::int4[] || '{3,1,2}'::int4[]) || ARRAY[t7.id, 10]), 1) AS c1, ((t7.a AT TIME ZONE 'UTC') + LEAST(t1.c, justify_hours(t1.c))) AS c2, CASE WHEN false THEN to_json('ab') ELSE '{"a": 1, "b": "x"}'::json END AS c3 FROM (v0 t6 LEFT JOIN t2 t7 ON t6.c1 = t7.b) WHERE CASE WHEN true THEN ('a,b' ~* 'b{1,2}') ELSE date_trunc('month', t7.a) IS NULL END) SELECT ln(abs(sum(t8.c0)) + 1) FROM w3 t8 GROUP BY t8.c1)));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-04', '0001-01-01 00:00:00 BC', '2024-02-29', 'P1DT12H', '0', '2', NULL), (2, NULL, NULL, '2025-03-09 03:30:00-04', '2024-02-29 00:00:00', '2000-03-01', '0', '0.5', NULL, NULL), (3, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', '2025-03-09 02:30:00', '-infinity', '-12:00', NULL, '-1.25', 1), (4, '"str"', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-10-26 01:30:00+00', '2024-02-29 00:00:00', '2025-01-01', '-1 mon 2 days', NULL, '-3', 11), (5, '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}', '2025-11-02 01:30:00-05', '2025-11-02 01:30:00', '1999-12-31', NULL, 'Infinity', '-0', 0);

INSERT INTO tp4 VALUES (-1, '-infinity', 'a', '99999999999999999999'), (-2, '2024-02-29', 'ab', '123.45'), (19, '4713-01-01 BC', NULL, '1e30'), (1, '2000-03-01', 'b', 'NaN'), (6, '2025-03-09', 'd', '-2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5ci", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 3, 'a-', 'Infinity', '1 year -1 sec', '[3,)', '{[1,3),[5,7)}', 'the and of', ARRAY[ROW(-2, 'ö')::r5ct]), (2, 17, 'a-', '-99999999999999999999999999999999999.999999', '178000000 years', '[5,6)', '{[0,100)}', 'Supernovae stars are the brightest phenomena in galaxies', NULL), (3, NULL, '-a', '0.0000000000000000000000000001', '2147483647 days', '[3,)', NULL, 'running runners ran; the runner runs', ARRAY[ROW(1, 'c')::r5ct]), (4, NULL, 'İ', '-99999999999999999999999999999999999.999999', '720:00:00', '(,2]', '{[1,3),[5,7)}', '', ARRAY[ROW(4, 'E')::r5ct, ROW(0, 'Ä')::r5ct]), (5, 0, 'AB', '-99999999999999999999999999999999999.999999', '-1 mon 30 days', '[3,)', '{}', 'fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42', '{}'), (6, 15, '9', '7', '720:00:00', '[-3,0]', '{}', 'running runners ran; the runner runs', '{}'), (7, 19, '', '-99999999999999999999999999999999999.999999', 'infinity', '[5,6)', '{}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(NULL, 'Ä')::r5ct, ROW(NULL, '9')::r5ct]), (8, 16, 'E', '2.5', NULL, '[0,10)', NULL, 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(1, 'å')::r5ct, ROW(-2, 'z')::r5ct]), (9, 11, 'SS', '-Infinity', 'infinity', 'empty', '{[1,3),[5,7)}', '', NULL), (10, -3, NULL, '7', '178000000 years', '(1,5]', '{}', 'Fat cats! FAT rats? fat-free', ARRAY[ROW(NULL, 'B')::r5ct]), (11, 3, 'å', 'NaN', '-178000000 years', '[3,)', '{[0,100)}', 'running runners ran; the runner runs', NULL);

CREATE INDEX r5a_iv ON r5a (iv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE UPDATE OF a, b ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > -1;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 19;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (2, 'a', NULL), (-2, 'b', '2025-01-01'), (1, 'c', 'infinity'), (4, 'd', '2024-02-29'), (-5, 'a', '2025-01-01'), (18, 'b', 'infinity') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (30, 'x0', NULL), (26, 'x1', NULL), (28, 'x2', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, NULL, '{"plan": "pro", "seats": 3}'), (2, 'u2@x.org', 'Éva2', 2, '{a}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'dee3', 3, '{b,NULL}', '{"plan": "pro", "seats": 3}'), (4, 'u4@x.org', '4', 4, '{a}', '[1, {"a": null}]'), (5, 'u5@x.org', 'dee5', 0, NULL, '{"seats": 1.50}'), (6, 'u6@x.org', 'dee6', 6, '{}', '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'bob 7', 7, '{a}', NULL), (8, 'u8@x.org', 'zoë8', 8, '{b,NULL}', '[1, {"a": null}]'), (9, 'u9@x.org', 'dee9', 9, '{a,b}', NULL);

INSERT INTO r6o (uid, amount, status, created, data) VALUES (4, 12345678.90, 'paid', '2025-02-21', NULL), (6, NULL, 'paid', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (1, 100, 'void', '2025-02-08', NULL), (9, -1.50, 'new', '2025-02-11', '{"items": [{"sku": "a", "qty": 2}]}'), (6, 0, 'new', '2025-02-25', '{"gift": true}'), (3, 9.99, 'paid', '2025-02-26', '{"gift": true}'), (4, 9.99, 'void', '2025-02-07', '{"items": []}'), (3, -1.50, 'paid', '2025-02-28', NULL), (6, 12345678.90, 'paid', '2025-02-02', NULL), (1, 9.99, 'new', '2025-02-19', NULL), (5, 9.99, 'void', '2025-02-01', '{"gift": true}'), (3, 9.99, 'new', '2025-02-18', '{"items": [{"sku": "a", "qty": 2}]}'), (8, 12345678.90, 'paid', '2025-02-18', '{"gift": true}'), (4, 9.99, 'new', '2025-02-19', '{"items": [{"sku": "a", "qty": 2}]}'), (5, NULL, 'paid', '2025-02-27', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 100, 'paid', '2025-02-12', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 180, 'NOTE 3', 1000, 2.25, NULL), (2, 1, 127, 'é', 0, 'NaN', '2025-03-09 07:00:00+00'), (3, 1, 77, 'n%5', NULL, -0, '2025-03-09 06:59:59+00'), (4, 1, NULL, 'NOTE 3', 3.333, 1e300, '2025-03-09 06:59:59+00'), (5, 1, 13, 'NOTE 3', 1000, 'Infinity', NULL), (6, 5, -48, 'NOTE 3', 0.5, 2.25, '2025-11-02 05:30:00+00'), (7, 3, 184, 'NOTE 3', 1000, -0, '2025-11-02 05:30:00+00'), (8, 1, NULL, 'note 1', 0.5, 'Infinity', '2025-03-09 07:00:00+00'), (9, 2, -11, '', -12.125, 0.5, '2025-11-02 05:30:00+00'), (10, 1, 116, 'note 2', 0.5, 2.25, '2024-02-29 12:00:00+00');

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

INSERT INTO r7t VALUES (1, 'ß', 'ß'), (2, 'ä b', 'ä b'), (3, 'ss', 'ss'), (4, 'E', 'E'), (5, 'ä', 'ä');

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

INSERT INTO r8a VALUES (1, 3, 0, -2.25, '', '2025-01-01'::timestamp + interval '142 hours'), (2, 1, 15, 1.50, '', '2025-01-01'::timestamp + interval '52 hours'), (3, 3, NULL, 10, 'b', '2025-01-01'::timestamp + interval '22 hours'), (4, 1, 3, -2.25, '', '2025-01-01'::timestamp + interval '7 hours'), (5, 3, NULL, 1.50, 'z', '2025-01-01'::timestamp + interval '81 hours'), (6, 4, 18, 10, NULL, '2025-01-01'::timestamp + interval '122 hours'), (7, 2, 10, 1.50, 'B', '2025-01-01'::timestamp + interval '10 hours'), (8, 1, NULL, 10, 'a', '2025-01-01'::timestamp + interval '210 hours'), (9, 1, 9, 0, 'b', '2025-01-01'::timestamp + interval '71 hours'), (10, 2, 19, 3.75, 'z', '2025-01-01'::timestamp + interval '66 hours'), (11, 1, 3, 1.50, NULL, '2025-01-01'::timestamp + interval '126 hours'), (12, 4, 9, NULL, 'z', '2025-01-01'::timestamp + interval '28 hours'), (13, 3, -3, 1.50, 'a', '2025-01-01'::timestamp + interval '28 hours'), (14, 3, 20, NULL, 'a', '2025-01-01'::timestamp + interval '108 hours'), (15, 1, 6, 10, 'a', '2025-01-01'::timestamp + interval '24 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 6, 9), (1, 2, 4), (2, 1, 2), (2, 2, 3), (3, 6, 5), (3, 3, 7), (5, 5, 2), (6, 4, 5), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{}', '(2024-12-31,2025-01-05]', NULL), (2, '(-3,1]', '{[-3,1), [3, 5)}', 'empty', '(,0]'), (3, '[-3,)', '{[-3,1), [3, 5)}', '(2024-12-31,2025-01-05]', '(,0]'), (4, '[8,12)', '{}', '[2025-01-01,2025-02-01)', NULL), (5, 'empty', NULL, '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (6, NULL, '{[-2,2), [4, 6)}', '[2025-01-01,2025-02-01)', NULL), (7, '[-3,)', '{(,-3)}', '[2025-03-01,infinity)', '[0,0]'), (8, '[3,)', NULL, '[2025-03-01,infinity)', '[1.5,2.5)'), (9, '[-1,3)', '{}', '[2025-01-01,2025-02-01)', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'über café naïve'), (2, 'running rats'), (3, 'a rat ran'), (4, 'quick quick slow'), (5, 'quick quick slow'), (6, 'foxes and dogs'), (7, 'brown''s fox''s den'), (8, 'foxes and dogs'), (9, 'running rats');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '"scalar"'), (2, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (3, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (4, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (5, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (6, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3771) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1652) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2662) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

CREATE TABLE r9a91616 (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text) WITH (autovacuum_enabled = off);

INSERT INTO r9a91616 SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % 3772, CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, 3772) i;

SET default_statistics_target = 25;

ALTER TABLE r9a91616 ALTER COLUMN g SET (n_distinct = -1);

CREATE STATISTICS r9a91616_st ON g, (u % 10) FROM r9a91616;

UPDATE r9a91616 SET g = 5, nul = NULL WHERE id % 50 = 0;

ANALYZE r9a91616;

RESET default_statistics_target;

-- statement
SELECT * FROM r9est($$SELECT * FROM r9a91616 WHERE arr @> '{2}'$$);
