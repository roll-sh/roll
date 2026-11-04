-- class: wrong_rows
-- ordered: false
-- note: 12 vs 11 rows
-- engine: QUERY PLAN:25
-- engine: Limit
-- engine:   ->  Sort
-- engine:         Disabled: true
-- engine:         Sort Key: a.id, b.id
-- engine:         ->  Hash Join
-- engine:               Hash Cond: (a.k = b.k)
-- engine:               Join Filter: (a.id < b.id)
-- engine:               ->  Seq Scan on r8big a
-- engine:                     Filter: ((id % 50) = 0)
-- engine:               ->  Hash
-- engine:                     ->  Seq Scan on r8big b
-- engine:                           Filter: ((id % 7) = 0)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Limit
-- reference:   ->  Incremental Sort
-- reference:         Sort Key: a.id, b.id
-- reference:         Presorted Key: a.id
-- reference:         ->  Nested Loop
-- reference:               Join Filter: ((a.id < b.id) AND (a.k = b.k))
-- reference:               ->  Index Scan using r8big_pkey on r8big a
-- reference:                     Filter: ((id % 50) = 0)
-- reference:               ->  Materialize
-- reference:                     ->  Seq Scan on r8big b
-- reference:                           Filter: ((id % 7) = 0)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a date, b date, c jsonb, g int GENERATED ALWAYS AS (coalesce(id, 0) * 2 + 1) VIRTUAL);

INSERT INTO t0 (id, a, b, c) VALUES (2, '2024-01-28', NULL, NULL), (18, NULL, '2024-02-09', NULL), (9, '2024-01-11', NULL, '{"a": "1"}'), (24, NULL, NULL, '[]'), (21, NULL, NULL, '{"a": 1, "b": "x"}'), (31, NULL, '2024-02-06', '{"b": [1, "a"]}'), (16, NULL, '2024-03-09', '{"a": {"b": 2}}'), (10, '2024-02-18', NULL, 'null'), (52, NULL, '2024-03-11', NULL), (40, '2024-03-08', NULL, NULL), (27, NULL, NULL, '[]'), (3, NULL, NULL, NULL), (37, NULL, NULL, '1'), (20, '2024-03-07', '2024-01-14', NULL), (4, NULL, NULL, NULL), (5, NULL, '2024-02-26', NULL), (8, NULL, '2024-03-05', '"s"'), (11, '2024-02-03', NULL, '{"b": [1, "a"]}'), (14, NULL, '2024-03-10', '{"a": 1, "b": "x"}'), (33, NULL, NULL, '{"a": 1, "b": "x"}');

CREATE INDEX t0_i703 ON t0 (id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER INSERT ON t0 REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (a int4[], b interval);

INSERT INTO t1 (a, b) VALUES ('{3,1,2}', '1 day'), (NULL, '24:00:00'), ('{1,1,2}', '2 hours'), ('{1,1,2}', '30 days'), (NULL, NULL), (NULL, '1 mon'), (NULL, '24:00:00'), ('{1,1,2}', '30 days'), ('{1,2}', '0'), ('{NULL,3}', '30 days'), (NULL, '2 hours'), ('{5}', NULL);

CREATE INDEX t1_i5 ON t1 (b);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN starts_with(('null'::jsonb ->> 'b'), replace(q, 'a', 'x')) THEN (CASE WHEN false THEN '{"a": 1, "b": "x"}'::jsonb ELSE '{"a": {"b": 2}}'::jsonb END::jsonb ->> 'a')::int ELSE ((p & p) - p) END $$;

CREATE FUNCTION f1(p int, q text) RETURNS int LANGUAGE plpgsql  STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF EXISTS (SELECT p FROM t0 t2) THEN
    r := (abs(10::int8))::int4;
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce(f0(p, q), 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE VIEW v0 AS SELECT '{ab}'::text[] AS c0 FROM (SELECT sum(7::int8) AS c0, sum(round((99.99::numeric)::numeric(6,1), 3)) AS c1, max(ceil(3::numeric)) FILTER (WHERE true) AS c2 FROM (t0 t2 RIGHT JOIN t1 t3 ON false) WHERE false) s1 WHERE NULLIF(NULL::bool, position('B' in 'Ab') NOT IN (SELECT 2 FROM t0 t4 WHERE (true) IS FALSE));

CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int);

CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k);

CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0);

CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v);

CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b');

CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT;

CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20);

CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT;

CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v);

INSERT INTO tj4 VALUES (1, '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', '{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}', NULL, '2025-10-26 02:30:00', NULL, '1 day', '2.5', '0.125', 8), (2, '{}', '{}', '2025-10-26 00:30:00+00', NULL, '1999-12-31', '36 hours', '-0.0', '0.5', 9), (3, NULL, NULL, '1999-12-31 23:59:59.999999+00', '2025-11-02 01:30:00', '2024-02-29', '36 hours', '-2.5', '2', 7), (4, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', NULL, '2025-10-26 02:30:00', '4713-01-01 BC', '-infinity', '123.45', '-0', 9);

INSERT INTO tp4 VALUES (-4, '2024-02-29', 'ab', '1.23456789012345678901234567890'), (15, '4713-01-01 BC', 'ab', 'NaN'), (9, 'infinity', 'd', '-0.0'), (19, '-infinity', 'd', '-7'), (-2, '1999-12-31', 'a', '1.23456789012345678901234567890'), (-4, '-infinity', NULL, 'NaN');

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "ucs_basic", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 5, 'o', '1e-130', '1 mon', '[5,6)', '{(,0],[10,)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(NULL, NULL)::r5ct]), (2, 4, '-a', NULL, '2147483647 mons', '[1,5)', '{[1,3),[5,7)}', 'The quick brown fox jumps over the lazy dog', '{}'), (3, 4, 'AB', '-Infinity', '2147483647 mons', '[0,10)', '{[0,100)}', 'The quick brown fox jumps over the lazy dog', NULL), (4, 14, 'Z', NULL, '178000000 years', '[1,5)', '{[0,100)}', 'The quick brown fox jumps over the lazy dog', ARRAY[ROW(NULL, 'B')::r5ct, ROW(1, 'a')::r5ct]), (5, 4, 'İ', '-0.5', '2147483647 mons', '[0,10)', '{(,0],[10,)}', 'PostgreSQL full text search: lexemes & weights', ARRAY[ROW(NULL, 'c')::r5ct, ROW(-1, ' a')::r5ct]), (6, 0, 'I', '7', '0', '[3,)', '{[2,4)}', 'Supernovae stars are the brightest phenomena in galaxies', ARRAY[ROW(3, 'B')::r5ct, ROW(-1, NULL)::r5ct, ROW(0, 'AB')::r5ct]), (7, -3, 'é', '2.5', '-178000000 years', '[1,5)', '{empty}', 'the and of', ARRAY[ROW(-2, 'é')::r5ct, ROW(0, 'Ab')::r5ct]), (8, NULL, 'ss', '1e-130', '30 days', '(,2]', '{[0,100)}', 'running runners ran; the runner runs', ARRAY[ROW(1, 'c')::r5ct, ROW(0, NULL)::r5ct]), (9, NULL, 'AB', '-0.5', '-2147483648 days', '[0,10)', '{[0,100)}', '', ARRAY[ROW(2, '')::r5ct, ROW(0, 'ab')::r5ct]), (10, 18, 'Ä', '2.5', NULL, '[5,6)', NULL, 'PostgreSQL full text search: lexemes & weights', NULL), (11, -2, 'ss', NULL, '-2147483648 days', '[0,10)', '{[0,100)}', NULL, ARRAY[ROW(5, NULL)::r5ct, ROW(4, NULL)::r5ct, ROW(0, 'ae')::r5ct]);

CREATE INDEX r5a_tsv ON r5a USING gin (tsv);

CREATE INDEX r5a_r ON r5a USING gist (r);

CREATE INDEX r5a_mr ON r5a USING gist (mr);

CREATE INDEX r5a_iv ON r5a (iv);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 5 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 3 WITH LOCAL CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 23 WITH CASCADED CHECK OPTION;

CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0;

CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a;

CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$;

CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt();

CREATE TABLE r5r (id int, a int, t text);

INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g;

CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t);

CREATE RULE r5r_upd AS ON UPDATE TO r5r DO INSTEAD INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a);

CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING;

CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k);

CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10);

CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v);

CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b');

CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT;

CREATE TABLE r5pd PARTITION OF r5p DEFAULT;

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (16, 'a', '2024-02-29'), (2, 'b', NULL) ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (19, 'x0', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'ann1', 1, NULL, '{"seats": 1.50}'), (2, 'u2@x.org', 'bob2', 2, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (3, 'u3@x.org', 'Éva3', 3, '{a}', '{"plan": "free", "tags": [1, 2]}'), (4, 'u4@x.org', 'Éva4', 0, '{}', '{}'), (5, 'u5@x.org', 'zoë5', 0, '{b,NULL}', '[1, {"a": null}]'), (6, 'u6@x.org', 'bob6', 6, NULL, '{"plan": "pro", "seats": 3}'), (7, 'u7@x.org', 'zoë7', 0, '{a,b}', '[1, {"a": null}]'), (8, 'u8@x.org', 'bob 8', 8, NULL, NULL), (9, 'u9@x.org', 'zoë9', 9, '{a}', '{"plan": "free", "tags": [1, 2]}'), (10, 'u10@x.org', 'bob 10', 10, NULL, '{"seats": 1.50}'), (11, 'u11@x.org', 'zoë11', 0, '{a}', '{}'), (12, 'u12@x.org', 'ann12', 12, '{a,b}', '[1, {"a": null}]');

INSERT INTO r6o (uid, amount, status, created, data) VALUES (7, NULL, 'paid', '2025-02-25', '{"gift": true}');

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

INSERT INTO r7k VALUES (1, 1, 167, 'é', 1000, NULL, NULL), (2, 1, 45, 'é', 3.333, 2.25, NULL), (3, 1, 148, 'note 3', 0, 'NaN', NULL), (4, 1, 169, 'é', 1000, 'NaN', '2025-03-09 07:00:00+00'), (5, 1, 1, 'note_4', 0.5, 2.25, '2025-03-09 06:59:59+00'), (6, 1, 155, 'NOTE 3', 0.5, NULL, '2025-03-09 07:00:00+00'), (7, 1, NULL, 'NOTE 3', 0, -0, '2024-02-29 12:00:00+00'), (8, 1, 51, 'note 3', -12.125, -0, '2025-03-09 06:59:59+00'), (9, 3, 27, 'Note 3', 0.5, 'NaN', '2025-11-02 05:30:00+00'), (10, 1, 63, 'note 2', 0.5, -0, '2025-03-09 07:00:00+00'), (11, 1, 16, 'note 1', NULL, 0.5, NULL), (12, 1, 65, '', 1000, 2.25, '2024-02-29 12:00:00+00'), (13, 1, 31, 'NOTE 3', 3.333, 1e300, '2025-03-09 06:59:59+00'), (14, 6, -16, 'NOTE 3', 3.333, NULL, '2024-02-29 12:00:00+00'), (15, 1, -2, '', -12.125, 'NaN', NULL), (16, 2, 145, 'note 3', 0, 'NaN', '2025-03-09 07:00:00+00'), (17, 8, NULL, 'n%5', -12.125, NULL, '2025-03-09 07:00:00+00'), (18, 1, 109, '', 3.333, NULL, '2025-11-02 05:30:00+00'), (19, 1, 190, 'note_4', -12.125, 2.25, '2025-03-09 07:00:00+00'), (20, 1, 55, 'é', 0.5, 'NaN', '2025-03-09 06:59:59+00'), (21, 1, 163, 'NOTE 3', 0.5, 2.25, '2025-03-09 07:00:00+00'), (22, 1, 149, 'Note 3', -12.125, 0.5, '2025-11-02 05:30:00+00'), (23, 5, -18, '', NULL, NULL, '2024-02-29 12:00:00+00');

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

CREATE TABLE r7g (id int GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL);

INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big');

CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0);

INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5);

CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci);

INSERT INTO r7t VALUES (1, '10', '10'), (2, 'ß', 'ß'), (3, '', ''), (4, 'E', 'E'), (5, 'ss', 'ss'), (6, 'é', 'é'), (7, '9', '9'), (8, '9', '9'), (9, 'e', 'e'), (10, 'ä b', 'ä b'), (11, 'ß', 'ß'), (12, 'A', 'A'), (13, 'ss', 'ss');

CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text);

INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL);

ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY;

ALTER TABLE r7acct FORCE ROW LEVEL SECURITY;

CREATE POLICY r7_own ON r7acct USING (owner = current_user);

CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0);

CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx');

CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us');

GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice;

GRANT SELECT ON r7acct TO r7_bob;

GRANT SELECT (id, grp, v) ON r7k TO r7_alice;

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, 18, 10, 'B', '2025-01-01'::timestamp + interval '60 hours'), (2, 3, 8, -2.25, '', '2025-01-01'::timestamp + interval '128 hours'), (3, 3, 4, 1.50, 'b', '2025-01-01'::timestamp + interval '213 hours'), (4, 4, 18, 1.50, 'a', '2025-01-01'::timestamp + interval '207 hours'), (5, 3, -1, 0, 'a', '2025-01-01'::timestamp + interval '144 hours'), (6, 4, 10, 3.75, 'ab', '2025-01-01'::timestamp + interval '102 hours'), (7, 4, 8, 1.50, 'b', '2025-01-01'::timestamp + interval '174 hours'), (8, 4, 11, 3.75, NULL, '2025-01-01'::timestamp + interval '47 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (3, 1, 6), (4, 5, 5), (4, 6, 9), (5, 5, 6), (6, 4, 6), (6, 3, 3), (7, 5, 8), (7, 4, 9), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', '{(,10)}', '(2024-12-31,2025-01-05]', NULL), (2, '[1,)', '{[1,7), [9, 11)}', 'empty', '(1.25,1.5)'), (3, NULL, '{}', '(2024-12-31,2025-01-05]', '(,0]'), (4, '[4,10)', NULL, '(2024-12-31,2025-01-05]', '(1.25,1.5)'), (5, '[9,13)', '{(,9)}', '(2024-12-31,2025-01-05]', '(,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'jumps over the lazy dog'), (2, 'running rats'), (3, 'quick quick slow'), (4, 'a rat ran'), (5, 'quick quick slow'), (6, 'running rats');

CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv);

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (2, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (3, '{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}'), (4, '"scalar"'), (5, '"scalar"');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 3980) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 1562) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3245) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

ANALYZE r9s;

ANALYZE r9n;

\s3 SET plan_cache_mode = force_generic_plan;

\x3 name=r9x42871 types=23,23 describe rf=0 p=b00000002 p=t'1'
SELECT n.id, n.a / $1 FROM r9n n JOIN r9s s ON s.id = n.id WHERE s.g = $2 ORDER BY n.id LIMIT 3;

\x3 name=r9x42871 p=b00000000 p=b00000003
-;

\x3 name=r9x42871 p=t'0' p=t'1'
-;

\x3 name=r9x42871 rf=1,0 p=t'0' p=t'5'
-;

\x3 name=r9x42871 rf=0 p=t'7' p=b00000063
-;

\x3 name=r9x42871 rf=1 p=b00000007 p=t'2'
-;

\x3 name=r9x42871 rf=1,0 p=b00000002 p=t'2'
-;

\x3 name=r9x42871 p=t'1' p=b00000002
-;

\x3 name=r9x42871 rf=1 p=b00000001 p=t'99'
-;

\s3 SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'r9x42871';

\s3 DEALLOCATE r9x42871;

\s3 RESET plan_cache_mode;

DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = force_custom_plan; FOR i IN 0..6 LOOP s := s + coalesce((SELECT count(*) FROM r9n WHERE a > i * 1000) / (i % 3), 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$;

\s2 SELECT pg_advisory_lock(4);

\s3 SELECT pg_try_advisory_lock(4), pg_try_advisory_lock_shared(4), pg_try_advisory_xact_lock(4);

\s3& SELECT pg_advisory_lock(4);

\s2 SELECT pg_advisory_unlock(4), pg_advisory_unlock(4);

\s3.;

\s3 SELECT locktype, mode, granted FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid();

\s3 SELECT pg_advisory_unlock_all();

\s2 SELECT pg_advisory_unlock_all();

\s2 ROLLBACK;

\s3 ROLLBACK;

SET work_mem = '256kB';

SET enable_sort = off;

-- statement
EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM r8big a JOIN r8big b ON a.k = b.k AND a.id < b.id WHERE a.id % 50 = 0 AND b.id % 7 = 0 ORDER BY a.id, b.id LIMIT 15;
