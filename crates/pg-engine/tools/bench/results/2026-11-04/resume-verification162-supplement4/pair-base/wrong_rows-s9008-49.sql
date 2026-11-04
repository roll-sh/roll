-- class: wrong_rows
-- ordered: false
-- note: 8 vs 10 rows
-- engine: QUERY PLAN:25
-- engine: Aggregate
-- engine:   ->  Append
-- engine:         ->  Seq Scan on r9i46504_1
-- engine:               Disabled: true
-- engine:               Filter: (a = ANY ('{1,2,96}'::integer[]))
-- engine:         ->  Seq Scan on r9i46504_2
-- engine:               Disabled: true
-- engine:               Filter: (a = ANY ('{1,2,96}'::integer[]))
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Aggregate
-- reference:   ->  Append
-- reference:         ->  Bitmap Heap Scan on r9i46504_1
-- reference:               Recheck Cond: (a = ANY ('{1,2,96}'::integer[]))
-- reference:               ->  Bitmap Index Scan on r9i46504_1_a_idx
-- reference:                     Index Cond: (a = ANY ('{1,2,96}'::integer[]))
-- reference:         ->  Bitmap Heap Scan on r9i46504_2
-- reference:               Recheck Cond: (a = ANY ('{1,2,96}'::integer[]))
-- reference:               ->  Bitmap Index Scan on r9i46504_2_a_idx
-- reference:                     Index Cond: (a = ANY ('{1,2,96}'::integer[]))
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a d3int, b timestamptz, c float8, d int2);

INSERT INTO t0 (id, a, b, c, d) VALUES (37, 1, '2024-01-03 23:00:00+00', 3.5, 0), (27, -1, NULL, 2.25, NULL), (20, NULL, '2024-01-01 06:00:00+00', NULL, 2), (35, 4, '2024-01-09 12:00:00+00', 2.25, 0), (25, NULL, '2024-01-05 12:00:00+00', 1.5, 1);

CREATE TABLE t1 (id int4 PRIMARY KEY, a numeric(10,2), b json);

INSERT INTO t1 (id, a, b) VALUES (15, NULL, NULL), (39, NULL, '{"a": "1"}'), (40, -2.05, NULL), (34, NULL, '{"a": 3}'), (6, 10.00, NULL), (10, NULL, '{"a": "1"}'), (47, NULL, '1'), (21, NULL, '{"b": [1, "a"]}'), (12, NULL, NULL), (1, NULL, NULL), (3, NULL, '{"a": "1"}'), (31, -3.00, NULL);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t1 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER UPDATE ON t1 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TYPE ct3 AS (a text COLLATE "en-x-icu", b int);

CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int);

INSERT INTO tc3 VALUES (1, ROW('B', -1)::ct3, ARRAY[ROW('Ä', 2)::ct3, ROW('x ', 1)::ct3], 1), (2, NULL, NULL, NULL), (3, NULL, '{}', 4), (4, ROW('A', 2)::ct3, NULL, NULL), (5, ROW(' x', 2)::ct3, '{}', 4), (6, ROW(NULL, 0)::ct3, NULL, NULL), (7, ROW('Ä', 3)::ct3, ARRAY[ROW('B', 1)::ct3, ROW('aa', -1)::ct3], 0), (8, NULL, ARRAY[ROW('', -1)::ct3, ROW(' x', 1)::ct3], NULL), (9, ROW('aa', 0)::ct3, NULL, 2), (10, ROW('', 2)::ct3, ARRAY[ROW('é', -1)::ct3, ROW('', 0)::ct3], 3), (11, ROW('ba', 1)::ct3, ARRAY[ROW('ß', 2)::ct3, ROW(NULL, 1)::ct3], 3), (12, ROW('ba', 0)::ct3, ARRAY[ROW('', 0)::ct3, ROW('z', 2)::ct3], NULL), (13, ROW('foo', NULL)::ct3, NULL, 4), (14, ROW(NULL, 1)::ct3, '{}', 1);

CREATE UNIQUE INDEX tc3_u ON tc3 (c);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE AS $$
DECLARE r int := 0;
BEGIN
  IF true THEN
    r := ((NULL::numeric - p))::int4;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(6, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT CASE WHEN false THEN ascii(NULLIF(q, 'ba')) ELSE extract(day from make_date(2024, 10, 1))::int END $$;

CREATE VIEW v0 AS SELECT to_json(upper('')) AS c0, t2.id AS c1, JSON_OBJECT('a': (jsonb_array_length('"s"'::jsonb) % 1), 'b': quote_literal(repeat('ba', 2))  RETURNING jsonb) AS c2, (false OR t2.id IS NULL) AS c3 FROM t1 t2 WHERE '2025-02-09'::date BETWEEN 'infinity'::date AND (make_date(2024, 5, 1) + 3);

CREATE VIEW v1 AS SELECT array_append('{ab}'::text[], '9') AS c0, t1.c1 AS c1, (((2::float8 + NULL::float8) + ('Infinity'::float8 * 2::float8)) + (extract(dow from '1999-03-02'::date)::int)::float8) AS c2 FROM (v0 t1 LEFT JOIN unnest('{2,NULL}'::int4[]) WITH ORDINALITY s2(u, o) ON t1.c1 = s2.u AND (array_length('{1,1,2}'::int4[], 1), COALESCE('Ab', 'ab')) <= (COALESCE(s2.u, t1.c1), 'foo'));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-03-09 03:30:00-04', '2024-02-29 00:00:00', '1999-12-31', '-infinity', '1.23456789012345678901234567890', '0.5', NULL), (2, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2038-01-19 03:14:08+00', NULL, 'infinity', '1 mon -30 days', '123.45', NULL, 1), (3, '{}', '{"a": 1, "a": 2,  "b" : [1,2]}', 'infinity', '0001-01-01 00:00:00 BC', '2024-02-29', '36 hours', NULL, '0.5', 2), (4, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', 'infinity', '1970-01-01 00:00:00.5', 'infinity', NULL, '-2.5', '1024', 11);

INSERT INTO tp4 VALUES (12, NULL, 'd', NULL), (17, NULL, 'ab', '1e30'), (20, '2025-01-01', 'ab', NULL), (12, '-infinity', 'b', '-1e-20'), (20, '2025-03-09', 'd', '-2.5');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "C", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 12, 'i', '-0.5', NULL, '(,)', '{[2,4)}', 'running runners ran; the runner runs', ARRAY[ROW(3, '')::r5ct]), (2, 7, 'i', '0', '1 day 00:00:00.000001', 'empty', '{(,0],[10,)}', 'PostgreSQL full text search: lexemes & weights', '{}'), (3, 11, 'A', '1e100', '2147483647 mons', 'empty', '{}', 'running runners ran; the runner runs', '{}'), (4, 10, NULL, 'Infinity', '-2147483648 days', 'empty', '{[0,100)}', 'a fat cat sat on a mat and ate a fat rat', NULL), (5, 8, NULL, '-0.5', NULL, '(1,5]', NULL, 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(NULL, 'a')::r5ct, ROW(3, 'å')::r5ct]), (6, 4, 'ch', NULL, '-2147483648 days', '[-3,0]', '{[1,2],[2,3)}', 'Supernovae stars are the brightest phenomena in galaxies', '{}');

CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 3 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 2 WITH CASCADED CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 25 WITH CHECK OPTION;

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

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20);

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30);

INSERT INTO r5p VALUES (0, 'a', 'infinity'), (6, 'b', '2024-02-29') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (30, 'x0', NULL), (27, 'x1', NULL), (27, 'x2', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', '1', 1, '{a}', '{"seats": 1.50}'), (2, 'u2@x.org', 'bob2', 2, '{a}', '{"plan": "pro", "seats": 3}'), (3, 'u3@x.org', 'bob 3', 3, '{b,NULL}', NULL), (4, 'u4@x.org', 'Éva4', 4, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (3, 12345678.90, 'paid', '2025-02-08', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 0, 'paid', '2025-02-08', '{"gift": true}'), (4, 12345678.90, 'new', '2025-02-03', '{"gift": true}'), (1, -1.50, 'paid', '2025-02-10', NULL), (3, 9.99, 'paid', '2025-02-09', '{"items": []}'), (2, NULL, 'void', '2025-02-16', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 9.99, 'paid', '2025-02-04', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 9.99, 'paid', '2025-02-04', '{"items": [{"sku": "a", "qty": 2}]}'), (3, 0, 'paid', '2025-02-23', '{"gift": true}'), (3, 0, 'paid', '2025-02-24', NULL), (1, -1.50, 'paid', '2025-02-12', '{"items": [{"sku": "a", "qty": 2}]}'), (1, -1.50, 'paid', '2025-02-01', NULL), (3, -1.50, 'paid', '2025-02-08', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 100, 'void', '2025-02-12', '{"items": []}'), (1, 12345678.90, 'paid', '2025-02-17', NULL), (3, 9.99, 'paid', '2025-02-01', NULL), (2, 9.99, 'void', '2025-02-25', '{"items": []}'), (1, NULL, 'new', '2025-02-19', '{"items": [{"sku": "a", "qty": 2}]}'), (2, -1.50, 'paid', '2025-02-12', '{"items": [{"sku": "a", "qty": 2}]}'), (3, -1.50, 'new', '2025-02-05', '{"gift": true}'), (3, NULL, 'new', '2025-02-22', '{"items": [{"sku": "a", "qty": 2}]}'), (2, 100, 'paid', '2025-02-15', '{"items": [{"sku": "a", "qty": 2}]}'), (4, 0, 'paid', '2025-02-23', NULL), (2, NULL, 'paid', '2025-02-22', '{"gift": true}'), (4, NULL, 'paid', '2025-02-14', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 23, 'é', -12.125, 0.5, '2025-03-09 06:59:59+00'), (2, 1, 73, 'NOTE 3', -12.125, 2.25, '2025-03-09 07:00:00+00'), (3, 7, 33, 'n%5', 0, NULL, '2024-02-29 12:00:00+00'), (4, 4, -14, 'note 3', NULL, NULL, NULL), (5, 3, 194, 'note 1', -12.125, 0.5, '2025-03-09 07:00:00+00'), (6, 1, 82, 'é', 1000, 0.5, '2025-11-02 05:30:00+00'), (7, 3, 91, 'NOTE 3', -12.125, -0, '2025-11-02 05:30:00+00'), (8, 1, 64, 'note 2', NULL, 'Infinity', '2025-03-09 06:59:59+00'), (9, 1, -28, 'note 2', 1000, 2.25, '2025-03-09 07:00:00+00'), (10, 1, -22, 'note 1', 0.5, 0.5, '2024-02-29 12:00:00+00'), (11, 1, -7, 'note 1', NULL, -0, '2025-11-02 05:30:00+00'), (12, 6, 66, 'note 3', -12.125, -0, '2025-03-09 06:59:59+00'), (13, 7, 181, 'é', NULL, NULL, '2025-03-09 07:00:00+00'), (14, 1, -24, 'note 1', 0, 'NaN', '2025-11-02 05:30:00+00');

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

INSERT INTO r7t VALUES (1, 'e', 'e'), (2, '9', '9'), (3, 'ss', 'ss'), (4, '10', '10'), (5, 'ae', 'ae'), (6, 'ä b', 'ä b'), (7, 'z', 'z'), (8, 'E', 'E');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

GRANT SELECT ON r7k TO r7_bob;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 1, 14, 10, 'b', '2025-01-01'::timestamp + interval '47 hours'), (2, 1, 0, 1.50, 'b', '2025-01-01'::timestamp + interval '111 hours'), (3, 4, 13, 3.75, '', '2025-01-01'::timestamp + interval '51 hours'), (4, 3, 3, NULL, NULL, '2025-01-01'::timestamp + interval '16 hours'), (5, 1, 9, 1.50, 'a', '2025-01-01'::timestamp + interval '66 hours'), (6, 1, NULL, 3.75, 'z', '2025-01-01'::timestamp + interval '69 hours'), (7, 3, 8, 1.50, NULL, '2025-01-01'::timestamp + interval '75 hours'), (8, 1, 5, NULL, NULL, '2025-01-01'::timestamp + interval '5 hours'), (9, 2, NULL, 1.50, '', '2025-01-01'::timestamp + interval '50 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 1), (2, 1, 5), (2, 1, 5), (4, 4, 2), (4, 4, 2), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, '[-1,5)', '{[-1,5), [7, 9)}', '(2024-12-31,2025-01-05]', '[0,0]'), (2, '(4,7]', '{[4,7), [9, 11)}', '[2025-01-01,2025-02-01)', '[0,0]'), (3, '[10,)', '{[10,14), [16, 18)}', NULL, NULL), (4, '[2,2)', NULL, '(2024-12-31,2025-01-05]', '(,0]'), (5, '(0,5]', '{(,0)}', NULL, '[1.5,2.5)'), (6, NULL, NULL, NULL, '[0,0]'), (7, '[5,)', '{(,5)}', NULL, '(,0]'), (8, '[-3,)', NULL, '[2025-01-01,2025-02-01)', '(,0]'), (9, NULL, '{(,1)}', '[2025-01-01,2025-02-01)', '(1.25,1.5)'), (10, '[1,)', '{(,1)}', '(2024-12-31,2025-01-05]', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'running rats'), (2, 'dog-eat-dog world'), (3, 'quick quick slow'), (4, 'quick quick slow'), (5, 'the quick brown fox'), (6, 'jumps over the lazy dog'), (7, 'foxes and dogs');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"a": 1e3, "n": -0.0, "big": 12345678901234567890}'), (2, '"scalar"'), (3, '"scalar"'), (4, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (5, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (6, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3441) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2265) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 2407) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

CREATE TABLE r8c86187 (id int NOT NULL, a text, b int DEFAULT 42, c date);

COPY r8c86187 FROM STDIN (FORMAT csv, LOG_VERBOSITY verbose, ON_ERROR stop);
1,,3,2025-01-31
2,,99999999999
3,,zz,2025-02-29
,multi word,zz,infinity
5,multi word,
\.

COPY r8c86187 TO STDOUT (FORMAT csv, QUOTE '''', ESCAPE '\');

COPY r8c86187 FROM STDIN (FORMAT binary, ON_ERROR ignore);

DROP TABLE r8c86187;

\s3 SET plan_cache_mode = force_generic_plan;

\x3 name=r9x49803 types=23 describe rf=1 p=null
SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g;

\x3 name=r9x49803 rows=2 p=t'49'
-;

\x3 name=r9x49803 rf=0 p=t'-1'
-;

\x3 name=r9x49803 p=b00000000b2d05e00
-;

\x3 name=r9x49803 rf=0 p=t'-50'
-;

\x3 name=r9x49803 rf=0 p=null
-;

\x3 name=r9x49803 p=t'0'
-;

\x3 name=r9x49803 p=b00000000b2d05e00
-;

\x3 name=r9x49803 rf=1,0 p=null
-;

\x3 name=r9x49803 p=t'-50'
-;

\x3 name=r9x49803 p=t'-50'
-;

\s3 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x49803';

\s3 DEALLOCATE r9x49803;

\s3 RESET plan_cache_mode;

VACUUM r9s;

BEGIN;

DECLARE r9cur SCROLL CURSOR FOR SELECT id, v FROM r9s WHERE id BETWEEN 1842 AND 2342 ORDER BY id ;

FETCH 3 FROM r9cur;

FETCH ABSOLUTE 2 FROM r9cur;

MOVE FORWARD ALL FROM r9cur;

CLOSE r9cur;

COMMIT;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = force_custom_plan; FOR i IN 0..11 LOOP s := s + coalesce((SELECT count(*) FROM r9s WHERE g = i % 9 AND v > 10 / (i % 6)), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

SET enable_sort = off;

RESET enable_sort;

CREATE TABLE r9i46504 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) PARTITION BY RANGE (id);

CREATE TABLE r9i46504_1 PARTITION OF r9i46504 FOR VALUES FROM (MINVALUE) TO (1000) WITH (autovacuum_enabled = off);

CREATE TABLE r9i46504_2 PARTITION OF r9i46504 FOR VALUES FROM (1000) TO (MAXVALUE) WITH (autovacuum_enabled = off);

INSERT INTO r9i46504 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 3130) i;

ANALYZE r9i46504;

CREATE INDEX ON r9i46504 USING brin (a) WITH (pages_per_range = 2);

CREATE UNIQUE INDEX ON r9i46504 USING btree (a);

CREATE INDEX ON r9i46504 USING btree (a) WHERE a > 50;

INSERT INTO r9i46504 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(3131, 3199) i;

UPDATE r9i46504 SET a = a + 1, t = t || 'u' WHERE id % 5 = 0;

DELETE FROM r9i46504 WHERE id % 11 = 0;

SET enable_seqscan = off;

-- statement
EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i46504 WHERE a = ANY ('{1,2,96}');
