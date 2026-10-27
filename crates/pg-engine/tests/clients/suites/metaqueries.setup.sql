-- A database with many object kinds for the pgAdmin / DBeaver metadata replays (a81_meta).
-- A statement the engine lacks fails here alone; the setup logs of both servers show which.
DROP DATABASE IF EXISTS a81_meta;
DROP ROLE IF EXISTS meta_owner;
DROP ROLE IF EXISTS meta_reader;
CREATE ROLE meta_owner LOGIN PASSWORD 'pw' CONNECTION LIMIT 5 VALID UNTIL '2100-01-01';
CREATE ROLE meta_reader NOLOGIN;
COMMENT ON ROLE meta_reader IS 'read only';
CREATE DATABASE a81_meta OWNER meta_owner TEMPLATE template0 ENCODING 'UTF8';
COMMENT ON DATABASE a81_meta IS 'metadata replay';
\c a81_meta
CREATE EXTENSION IF NOT EXISTS hstore;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS file_fdw;
CREATE SCHEMA app AUTHORIZATION meta_owner;
CREATE SCHEMA other;
COMMENT ON SCHEMA app IS 'the app';
GRANT USAGE ON SCHEMA app TO meta_reader;
ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO meta_reader;
SET search_path = app, public;
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE TYPE pair AS (a int, b text);
CREATE TYPE floatrange AS RANGE (subtype = float8, subtype_diff = float8mi);
CREATE DOMAIN posint AS int NOT NULL DEFAULT 1 CONSTRAINT posint_check CHECK (VALUE > 0);
COMMENT ON DOMAIN posint IS 'positive';
CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE SEQUENCE seq_a INCREMENT 5 START 100 MINVALUE 1 MAXVALUE 100000 CACHE 1 CYCLE;
COMMENT ON SEQUENCE seq_a IS 'a sequence';
CREATE TABLE parent (
  id bigint GENERATED ALWAYS AS IDENTITY (START 10 INCREMENT 2) PRIMARY KEY,
  code text COLLATE "C" NOT NULL UNIQUE,
  label varchar(40) DEFAULT 'none',
  m mood DEFAULT 'ok',
  p pair,
  r floatrange,
  amount numeric(12, 3) CHECK (amount >= 0),
  n posint,
  tags text[],
  attrs hstore,
  doc jsonb DEFAULT '{}'::jsonb,
  born timestamptz DEFAULT now(),
  t time(3) with time zone,
  iv interval day to second,
  bits bit varying(8),
  ip inet,
  u uuid,
  total numeric GENERATED ALWAYS AS (amount * 2) STORED,
  ci_name text COLLATE ci,
  s int DEFAULT nextval('seq_a')
) WITH (fillfactor = 80);
COMMENT ON TABLE parent IS 'the parent';
COMMENT ON COLUMN parent.code IS 'business key';
ALTER TABLE parent ALTER COLUMN label SET STATISTICS 200;
ALTER TABLE parent ALTER COLUMN doc SET STORAGE EXTERNAL;
CREATE TABLE child (
  id serial PRIMARY KEY,
  parent_id bigint NOT NULL REFERENCES parent (id) ON DELETE CASCADE ON UPDATE SET NULL DEFERRABLE INITIALLY DEFERRED,
  qty int NOT NULL DEFAULT 0,
  during tsrange,
  note text,
  CONSTRAINT qty_ok CHECK (qty >= 0) NOT VALID,
  CONSTRAINT child_uq UNIQUE (parent_id, qty) INCLUDE (note),
  EXCLUDE USING gist (during WITH &&)
);
CREATE INDEX child_note_trgm ON child USING gin (note gin_trgm_ops);
CREATE INDEX child_lower_note ON child (lower(note) DESC NULLS LAST) WHERE qty > 0;
CREATE INDEX parent_doc ON parent USING gin (doc jsonb_path_ops);
CREATE INDEX parent_born_brin ON parent USING brin (born);
CREATE INDEX parent_code_hash ON parent USING hash (code);
CREATE UNIQUE INDEX parent_u ON parent (u) NULLS NOT DISTINCT;
COMMENT ON INDEX parent_u IS 'uuid index';
CREATE TABLE measure (id int, at date NOT NULL, v float8) PARTITION BY RANGE (at);
CREATE TABLE measure_2025 PARTITION OF measure FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE measure_def PARTITION OF measure DEFAULT;
CREATE TABLE hlist (k text, v int) PARTITION BY LIST (k);
CREATE TABLE hlist_ab PARTITION OF hlist FOR VALUES IN ('a', 'b');
CREATE TABLE base_t (id int, created date);
CREATE TABLE derived_t (extra text) INHERITS (base_t);
CREATE UNLOGGED TABLE scratch (x int);
CREATE TABLE typed_t OF pair;
CREATE VIEW parent_v AS SELECT id, code, label FROM parent WHERE amount > 0 WITH CASCADED CHECK OPTION;
COMMENT ON VIEW parent_v IS 'a view';
CREATE VIEW parent_sec WITH (security_barrier = true) AS SELECT id FROM parent;
CREATE MATERIALIZED VIEW parent_mv AS SELECT m, count(*) AS n FROM parent GROUP BY m WITH DATA;
CREATE INDEX parent_mv_m ON parent_mv (m);
CREATE FUNCTION add_one(i int) RETURNS int LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS 'SELECT i + 1';
COMMENT ON FUNCTION add_one(int) IS 'adds one';
CREATE FUNCTION set_of(n int, OUT a int, OUT b text) RETURNS SETOF record LANGUAGE sql STABLE
  AS 'SELECT g, g::text FROM generate_series(1, n) g';
CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = app
  AS $$ BEGIN NEW.note := coalesce(NEW.note, 'x'); RETURN NEW; END $$;
CREATE PROCEDURE bump(IN by int DEFAULT 1) LANGUAGE plpgsql AS $$ BEGIN UPDATE child SET qty = qty + by; END $$;
CREATE TRIGGER child_touch BEFORE INSERT OR UPDATE OF note ON child FOR EACH ROW WHEN (NEW.qty > 0) EXECUTE FUNCTION touch();
CREATE TRIGGER child_stmt AFTER DELETE ON child REFERENCING OLD TABLE AS gone FOR EACH STATEMENT EXECUTE FUNCTION touch();
ALTER TABLE child DISABLE TRIGGER child_stmt;
CREATE AGGREGATE sum_sq(int) (SFUNC = int4pl, STYPE = int, INITCOND = '0');
CREATE FUNCTION mood_eq(mood, mood) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT $1 = $2';
CREATE OPERATOR === (LEFTARG = mood, RIGHTARG = mood, FUNCTION = mood_eq, COMMUTATOR = ===);
CREATE CAST (pair AS text) WITH INOUT;
CREATE RULE parent_v_ins AS ON INSERT TO parent_v DO INSTEAD INSERT INTO parent (code, label) VALUES (NEW.code, NEW.label);
ALTER TABLE child ENABLE ROW LEVEL SECURITY;
CREATE POLICY child_pos ON child AS PERMISSIVE FOR SELECT TO meta_reader USING (qty > 0);
CREATE TEXT SEARCH DICTIONARY simple_d (TEMPLATE = simple, STOPWORDS = english);
CREATE TEXT SEARCH CONFIGURATION cfg (COPY = pg_catalog.english);
ALTER TEXT SEARCH CONFIGURATION cfg ALTER MAPPING FOR asciiword WITH simple_d;
CREATE SERVER files FOREIGN DATA WRAPPER file_fdw;
CREATE USER MAPPING FOR meta_owner SERVER files;
CREATE FOREIGN TABLE ext_csv (a int, b text) SERVER files OPTIONS (filename '/dev/null', format 'csv');
CREATE STATISTICS parent_st (dependencies, ndistinct) ON code, label FROM parent;
CREATE PUBLICATION pub_app FOR TABLE parent, child WITH (publish = 'insert, update');
CREATE FUNCTION evt() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
CREATE EVENT TRIGGER evt_ddl ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION evt();
GRANT SELECT, INSERT ON parent TO meta_reader;
GRANT SELECT (code), UPDATE (label) ON parent TO meta_owner;
GRANT EXECUTE ON FUNCTION add_one(int) TO meta_reader;
REVOKE EXECUTE ON FUNCTION set_of(int) FROM PUBLIC;
ALTER TABLE parent OWNER TO meta_owner;
CREATE TABLE other.o1 (id int PRIMARY KEY, p bigint REFERENCES app.parent (id));
INSERT INTO parent (code, label, amount, n, tags, u) SELECT 'c' || g, 'l' || g, g, g, ARRAY['x'], md5('u' || g)::uuid FROM generate_series(1, 20) g;
INSERT INTO child (parent_id, qty, note) SELECT 10 + 2 * (g % 5), g, 'note ' || g FROM generate_series(1, 30) g;
ANALYZE;
