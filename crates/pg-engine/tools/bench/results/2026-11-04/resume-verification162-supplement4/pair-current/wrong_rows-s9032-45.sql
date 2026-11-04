-- class: wrong_rows
-- ordered: false
-- note: 5 vs 4 rows
-- engine: QUERY PLAN:25
-- engine: Aggregate
-- engine:   ->  Bitmap Heap Scan on r9i51071
-- engine:         Recheck Cond: (t = 'b4'::text)
-- engine:         ->  Bitmap Index Scan on r9i51071_x1
-- engine:               Index Cond: (t = 'b4'::text)
-- engine: -- EXPLAIN
-- reference: QUERY PLAN:25
-- reference: Aggregate
-- reference:   ->  Seq Scan on r9i51071
-- reference:         Disabled: true
-- reference:         Filter: (t = 'b4'::text)
-- reference: -- EXPLAIN

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a numeric(10,2), b text[], c int4, d numeric(10,2));

INSERT INTO t0 (id, a, b, c, d) VALUES (11, 0.50, '{a,b}', 6, 11.50), (30, 12.05, '{b,NULL}', NULL, -2.00), (14, NULL, '{z}', NULL, 12.75), (35, 8.25, '{a,b}', 7, NULL), (27, NULL, NULL, 6, NULL), (18, NULL, NULL, 1, -1.50), (33, 8.25, '{ab}', NULL, 2.05), (13, 3.00, '{b,NULL}', 9, 7.05), (31, NULL, NULL, 4, NULL), (16, NULL, NULL, 4, NULL), (32, 6.00, '{ab}', 6, 10.25), (7, 12.50, '{b,NULL}', -1, NULL);

CREATE INDEX t0_i799 ON t0 (b);

CREATE INDEX t0_i536 ON t0 (c);

CREATE TABLE t1 (id int4, a numeric(10,2), b float8, c json, d text[], e text[]);

INSERT INTO t1 (id, a, b, c, d, e) VALUES (21, NULL, 0.75, '"s"', NULL, '{z}'), (25, NULL, -0.25, '1', '{}', '{}'), (30, -2.50, NULL, '{"a": "1"}', NULL, NULL), (20, NULL, 3, NULL, '{a,b}', '{a,b}'), (15, 0.05, -2, NULL, '{z}', '{a,b}'), (1, 7.05, NULL, NULL, '{b,NULL}', '{ab}'), (26, 5.50, NULL, '"s"', NULL, '{ab}'), (40, 0.50, NULL, NULL, NULL, NULL), (22, NULL, 1.75, '{"a": {"b": 2}}', '{b,NULL}', '{A,a}'), (60, 6.05, 0.25, '{"a": 1, "b": "x"}', '{a,b}', '{A,a}'), (33, NULL, NULL, '1', '{a,b}', '{}'), (2, 10.00, 2.75, 'null', '{b,NULL}', '{a,b}'), (14, NULL, NULL, NULL, NULL, '{ab}'), (9, 6.00, -1.5, '{"a": 3}', '{A,a}', '{ab}'), (37, NULL, 1.25, 'null', '{A,a}', '{}'), (47, NULL, NULL, NULL, '{}', NULL), (4, 2.00, -0.5, NULL, '{}', '{}'), (32, -1.00, 4, '1', '{b,NULL}', '{ab}'), (5, NULL, NULL, NULL, NULL, NULL), (18, 5.00, NULL, NULL, NULL, NULL), (8, NULL, 1.5, '{"b": [1, "a"]}', NULL, '{ab}'), (42, 5.75, -2, NULL, NULL, NULL), (39, 5.00, 2.5, '{"a": {"b": 2}}', NULL, '{a,b}'), (11, NULL, NULL, NULL, '{}', '{ab}'), (29, -1.50, NULL, '{"a": 3}', NULL, '{z}'), (7, NULL, 3, NULL, NULL, NULL), (31, 11.50, NULL, NULL, NULL, '{A,a}'), (12, 3.75, NULL, NULL, NULL, NULL), (54, 10.75, 1, NULL, '{ab}', NULL), (52, 10.00, 0.5, '{"a": 3}', '{ab}', NULL);

CREATE UNIQUE INDEX t1_i140 ON t1 (id) NULLS NOT DISTINCT;

CREATE INDEX t1_i579 ON t1 (b);

ANALYZE;

CREATE FUNCTION f0(p int, q text) RETURNS int LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE r int := 0;
BEGIN
  IF false THEN
    r := width_bucket(1.5::numeric, 0, 10, 4);
  ELSE
    FOR i IN 1..least(coalesce(p, 0), 4) LOOP
      r := r + i;
    END LOOP;
    r := r + coalesce((COALESCE(p, p))::text::int4, 0);
  END IF;
  RETURN r;
EXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN
  RETURN -1;
END
$$;

CREATE FUNCTION srf0(lim int) RETURNS TABLE(x int) LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id FROM t0 WHERE id <= lim;
  IF lim > 3 THEN
    RETURN QUERY SELECT id FROM t0 WHERE id IS NULL;
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

INSERT INTO tj4 VALUES (1, '[1, 2, {"a": 3}]', '[1, 2, {"a": 3}]', '-infinity', '2024-02-29 00:00:00', '2025-03-09', 'infinity', '123.45', 'NaN', 10), (2, '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', '{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}', NULL, '0001-01-01 00:00:00 BC', '2000-03-01', '-1 year -1 day', '-2.5', NULL, 10), (3, '{}', '{}', 'infinity', '0001-01-01 00:00:00 BC', 'infinity', '1 mon -30 days', '0.000001', '2', 1), (4, '{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2024-02-29 23:00:00+00', '2025-03-09 02:30:00', '2025-03-09', NULL, '0.5', NULL, -3), (5, '{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}', '{"a": 1, "a": 2,  "b" : [1,2]}', '2025-11-02 01:30:00-05', 'infinity', '2025-03-09', '36 hours', '1e30', '0.5', 10);

INSERT INTO tp4 VALUES (5, '2000-03-01', 'c', '-0.0'), (23, '-infinity', 'd', NULL), (6, '-infinity', 'a', '-2.5'), (4, '2025-03-09', 'ab', '1.23456789012345678901234567890'), (13, '2025-03-09', 'b', '123.45'), (19, '2025-01-01', 'c', '0.5'), (24, '-infinity', 'd', '2.5'), (24, '4713-01-01 BC', NULL, '-7'), (7, '-infinity', 'ab', 'Infinity'), (18, '4713-01-01 BC', 'a', '-7'), (-1, '2000-03-01', NULL, NULL), (14, '2025-01-01', 'c', '99999999999999999999'), (4, '-infinity', 'b', 'NaN'), (9, NULL, NULL, '123.45');

ANALYZE tj4, tp4;

CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);

CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true');

CREATE TYPE r5ct AS (x int, y text);

CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE "r5num", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED);

CREATE TABLE r5log (tg text, op text, k int, v text);

CREATE TABLE r5b (k int PRIMARY KEY, w text);

INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g;

INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES (1, 15, NULL, '-0.5', '0', '(,2]', '{[2,4)}', 'the and of', ARRAY[ROW(2, 'z')::r5ct, ROW(0, 'ab')::r5ct]), (2, 13, 'é', NULL, '2147483647 mons', '(,2]', '{[2,4)}', NULL, '{}'), (3, NULL, '9', '-1e40', '2147483647 mons', '(,2]', '{}', 'the and of', '{}'), (4, 12, NULL, '1e-130', '-infinity', '[5,6)', '{[1,3),[5,7)}', 'a fat cat sat on a mat and ate a fat rat', NULL);

CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 6 AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$;

CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$;

CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$;

CREATE TRIGGER r5tb BEFORE INSERT OR UPDATE ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb();

CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION r5ta();

CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts();

CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > 4 WITH CHECK OPTION;

CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < 17;

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

CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date);

INSERT INTO r5p VALUES (11, 'a', '2024-02-29'), (13, 'b', '2025-01-01'), (-2, 'c', '2025-01-01'), (14, 'd', 'infinity'), (5, 'a', '2025-01-01') ON CONFLICT DO NOTHING;

INSERT INTO r5px VALUES (24, 'x0', NULL);

ANALYZE r5a, r5b, r5p;

CREATE TABLE r6u (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0, tags text[], meta jsonb, version int NOT NULL DEFAULT 1);

CREATE UNIQUE INDEX r6u_name ON r6u (name) WHERE n > 0;

CREATE TABLE r6o (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, uid int REFERENCES r6u (id) ON DELETE CASCADE, amount numeric(10, 2), status text CHECK (status IN ('new', 'paid', 'void')), created date, data jsonb);

CREATE INDEX r6o_uid ON r6o (uid, created);

INSERT INTO r6u (id, email, name, n, tags, meta) VALUES (1, 'u1@x.org', 'dee1', 0, '{}', '[1, {"a": null}]'), (2, 'u2@x.org', 'ann2', 2, NULL, '[1, {"a": null}]'), (3, 'u3@x.org', 'ann3', 3, '{b,NULL}', '{"seats": 1.50}'), (4, 'u4@x.org', 'bob 4', 4, '{a}', '{"seats": 1.50}'), (5, 'u5@x.org', 'Cid5', 5, '{b,NULL}', NULL), (6, 'u6@x.org', 'zoë6', 6, '{a,b}', '[1, {"a": null}]'), (7, 'u7@x.org', 'ann7', 7, '{b,NULL}', '{"seats": 1.50}'), (8, 'u8@x.org', 'Cid8', 8, '{b,NULL}', '{"plan": "free", "tags": [1, 2]}'), (9, 'u9@x.org', 'ann9', 0, '{a,b}', NULL), (10, 'u10@x.org', 'ann10', 10, NULL, NULL);

INSERT INTO r6o (uid, amount, status, created, data) VALUES (7, 100, 'paid', '2025-02-05', '{"items": [{"sku": "a", "qty": 2}]}'), (5, 0, 'void', '2025-02-05', NULL), (7, 9.99, 'new', '2025-02-16', NULL);

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

INSERT INTO r7k VALUES (1, 8, 112, 'NOTE 3', 0.5, 2.25, '2025-03-09 07:00:00+00'), (2, 1, 10, 'Note 3', 3.333, 'Infinity', '2025-03-09 07:00:00+00'), (3, 3, 10, 'é', 0, 1e300, '2025-03-09 07:00:00+00'), (4, 3, 59, 'note 2', 0, -0, '2025-11-02 05:30:00+00'), (5, 1, 87, 'note 3', 3.333, 0.5, '2025-03-09 07:00:00+00'), (6, 1, 55, 'n%5', -12.125, 2.25, NULL), (7, 1, -23, 'Note 3', 1000, 0.5, '2025-11-02 05:30:00+00'), (8, 4, 60, 'note 3', 3.333, 'NaN', '2025-03-09 06:59:59+00'), (9, 1, 18, '', -12.125, 2.25, '2025-03-09 07:00:00+00'), (10, 7, 192, 'n%5', 0.5, 'Infinity', '2025-11-02 05:30:00+00'), (11, 1, 162, 'note 2', -12.125, 0.5, NULL), (12, 1, NULL, '', 0, NULL, '2025-11-02 05:30:00+00'), (13, 8, NULL, 'Note 3', 1000, NULL, '2025-11-02 05:30:00+00'), (14, 1, 110, 'NOTE 3', 0.5, 'NaN', '2025-03-09 06:59:59+00'), (15, 4, 104, '', NULL, 'NaN', '2024-02-29 12:00:00+00'), (16, 1, 87, 'NOTE 3', 1000, 'NaN', '2025-11-02 05:30:00+00'), (17, 1, 186, 'NOTE 3', -12.125, 0.5, NULL), (18, 1, -41, 'note 1', 3.333, 'Infinity', '2024-02-29 12:00:00+00'), (19, 1, 182, 'n%5', -12.125, 1e300, '2025-03-09 07:00:00+00'), (20, 1, 194, 'note 3', 1000, 1e300, '2025-03-09 06:59:59+00');

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

INSERT INTO r7t VALUES (1, 'ss', 'ss'), (2, 'ß', 'ß'), (3, 'é', 'é'), (4, 'b', 'b'), (5, 'ß', 'ß'), (6, '', ''), (7, 'ß', 'ß'), (8, 'é', 'é'), (9, 'b', 'b'), (10, 'ae', 'ae'), (11, 'E', 'E');

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

ANALYZE r7k;

ANALYZE r7p;

ANALYZE r7r;

ANALYZE r7g;

ANALYZE r7d;

ANALYZE r7t;

ANALYZE r7acct;

CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp);

CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text);

INSERT INTO r8a VALUES (1, 3, 17, 0, '', '2025-01-01'::timestamp + interval '195 hours'), (2, 1, -3, 10, 'ab', '2025-01-01'::timestamp + interval '141 hours'), (3, 4, 6, 3.75, NULL, '2025-01-01'::timestamp + interval '42 hours'), (4, 4, -1, 1.50, 'b', '2025-01-01'::timestamp + interval '80 hours'), (5, 4, 8, 1.50, '', '2025-01-01'::timestamp + interval '24 hours'), (6, 4, 2, 1.50, NULL, '2025-01-01'::timestamp + interval '84 hours');

CREATE TABLE r8edge (src int, dst int, w int);

INSERT INTO r8edge VALUES (1, 2, 9), (4, 2, 1), (5, 4, 3), (1, 2, 1);

CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange);

INSERT INTO r8rng VALUES (1, 'empty', NULL, '(2024-12-31,2025-01-05]', '[1.5,2.5)'), (2, '[-1,0)', '{[-1,0), [2, 4)}', NULL, NULL), (3, 'empty', '{}', 'empty', '[0,0]'), (4, '[8,)', '{[8,8), [10, 12)}', NULL, '(,0]'), (5, NULL, '{(,2)}', '[2025-01-01,2025-02-01)', '[0,0]');

CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED);

INSERT INTO r8doc (id, body) VALUES (1, 'quick quick slow'), (2, ''), (3, 'quick quick slow'), (4, ''), (5, 'a rat ran');

CREATE TABLE r8j (id int PRIMARY KEY, j jsonb);

INSERT INTO r8j VALUES (1, '[1, "two", 3.5, null, true, {"k": [10, 20]}]'), (2, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (3, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}'), (4, '{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}'), (5, '{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}');

CREATE TABLE r8big (id int PRIMARY KEY, k int, s text);

INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, 2110) i;

ANALYZE r8a;

ANALYZE r8edge;

ANALYZE r8rng;

ANALYZE r8doc;

ANALYZE r8j;

ANALYZE r8big;

CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4)) WITH (autovacuum_enabled = off);

INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), round(i / 7.0, 4) FROM generate_series(1, 2017) i;

CREATE INDEX r9s_gv ON r9s (g, v);

CREATE INDEX r9s_t ON r9s (t);

CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, f float8, t text, tc text COLLATE "C", ti text COLLATE "en-x-icu", z int) WITH (autovacuum_enabled = off);

INSERT INTO r9n SELECT i, CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END FROM generate_series(1, 3572) i, LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t);

CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP RETURN NEXT regexp_replace(l, '\(cost=[0-9.]+\.\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\)', '(\1)'); END LOOP; END $$;

CREATE INDEX r9n_a ON r9n (a);

ANALYZE r9s;

ANALYZE r9n;

MERGE INTO r7g t USING (VALUES (6, 150), (10, 7)) s(id, v) ON t.a = s.id WHEN MATCHED THEN UPDATE SET a = s.v, t = 'm' || s.id WHEN NOT MATCHED BY TARGET AND s.v > 100 THEN DO NOTHING WHEN NOT MATCHED THEN INSERT (a, t) VALUES (s.v, 'n') RETURNING merge_action(), old.*, new.*;

SET extra_float_digits = 0;

SET TimeZone = 'UTC';

SET extra_float_digits = 1;

SELECT lo_from_bytea(951025, '\x00ff00ff')::int8 - 951025;

SELECT lo_get(951025), lo_get(951025, 2, 3);

BEGIN;

SELECT lo_open(951025, x'60000'::int);

SELECT loread(0, 3);

SELECT lo_tell(0), lo_lseek64(0, 0, 2);

SELECT lo_truncate(0, 0);

SELECT loread(0, 100);

SELECT lo_close(0);

ROLLBACK;

SELECT lo_get(951025);

SELECT lo_put(951025, 10, '\x7171');

SELECT lo_get(951025), length(lo_get(951025));

SELECT oid::int8 - 951025, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = 951025;

ALTER LARGE OBJECT 951025 OWNER TO r7_alice;

SELECT lo_unlink(951025);

SELECT lo_get(951025);

GRANT SELECT ON r7k TO PUBLIC;

SET ROLE r7_alice;

UPDATE r7acct SET region = 'xx' WHERE id = 1 RETURNING id;

INSERT INTO r7acct (id, amount, region) VALUES (6, -1, 'eu');

SELECT id, owner, amount FROM r7acct ORDER BY id;

RESET ROLE;

SELECT JSON_VALUE(jsonb '"scalar"', '$.a.type()' PASSING 1 AS x RETURNING jsonb NULL ON EMPTY);

CREATE TABLE r9i51071 (id int, a int, t text, arr int[], j jsonb, r int4range, p point) WITH (autovacuum_enabled = off);

INSERT INTO r9i51071 SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series(1, 2682) i;

CREATE INDEX CONCURRENTLY r9i51071_x0 ON r9i51071 USING gin (j jsonb_path_ops);

CREATE INDEX r9i51071_x1 ON r9i51071 USING btree (t COLLATE "C");

CREATE INDEX ON r9i51071 USING hash (a);

ALTER TABLE r9i51071 ADD CONSTRAINT r9i51071_ex EXCLUDE USING gist (p WITH ~=);

SET enable_seqscan = off;

EXPLAIN (COSTS OFF) SELECT id, a FROM r9i51071 WHERE a < 51 ORDER BY a DESC NULLS FIRST, id LIMIT 10;

-- statement
EXPLAIN (COSTS OFF) SELECT count(*), sum(id), min(t) FROM r9i51071 WHERE t = 'b4';
