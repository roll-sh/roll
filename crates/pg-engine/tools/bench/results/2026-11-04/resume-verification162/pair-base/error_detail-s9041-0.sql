-- class: error_detail
-- ordered: false
-- note: 0A000
-- engine: ERROR 0A000: generation expression uses user-defined type
-- engine: DETAIL: Virtual generated columns that make use of user-defined types are not yet supported.
-- engine: POSITION: 117
-- reference: ERROR 0A000: generation expression uses user-defined type
-- reference: DETAIL: Virtual generated columns that make use of user-defined types are not yet supported.
-- reference: POSITION: 126

CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);

CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3);

CREATE DOMAIN d3text AS text COLLATE "en-x-icu" CHECK (VALUE <> 'zz');

CREATE TABLE t0 (id int4 PRIMARY KEY, a int2, b d3text);

INSERT INTO t0 (id, a, b) VALUES (23, 3, 'abc'), (36, 5, 'Z'), (25, 1, 'ß'), (32, 1, NULL), (26, 3, 'A');

CREATE INDEX t0_i354 ON t0 ((id + 1));

CREATE INDEX t0_i340 ON t0 (a, id);

CREATE TABLE audit3 (op text, k int, v text);

CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = 2 THEN RETURN NULL; END IF; RETURN NEW; END $$;

CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3b();

CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$;

CREATE TRIGGER t3a AFTER INSERT OR DELETE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE TRIGGER t3u AFTER UPDATE ON t0 FOR EACH ROW EXECUTE FUNCTION trg3a();

CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$;

CREATE TRIGGER t3s AFTER DELETE ON t0 REFERENCING OLD TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s();

CREATE TABLE t1 (id int4 PRIMARY KEY, a int4 DEFAULT 5, b d3text, c int4);

INSERT INTO t1 (id, a, b, c) VALUES (30, NULL, 'ab', NULL), (36, 6, 'A', 7), (22, -1, 'Z', 7), (34, 2, NULL, 4), (31, 2, '10', NULL);

-- statement
CREATE TABLE t2 (a d3text, b numeric(10,2), c int8, d text COLLATE "und-x-icu", e d3int, g int GENERATED ALWAYS AS (coalesce(e, 0) * 2 + 1) VIRTUAL);
