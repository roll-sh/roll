-- pg_restore's source database a263_restore_src: many object kinds with data, as an application
-- schema migrated from PostgreSQL has them. Roles first (pg_restore sets owners and grants).
DROP DATABASE IF EXISTS a263_restore_src;
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'a263_owner') THEN CREATE ROLE a263_owner; END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'a263_reader') THEN CREATE ROLE a263_reader; END IF;
END $$;
CREATE DATABASE a263_restore_src;
\c a263_restore_src
CREATE EXTENSION citext;
CREATE EXTENSION hstore;
CREATE EXTENSION pg_trgm;
CREATE EXTENSION btree_gist;
CREATE SCHEMA app AUTHORIZATION a263_owner;
CREATE SCHEMA audit;
COMMENT ON SCHEMA app IS 'the application';
SET search_path = app, public;
CREATE TYPE status AS ENUM ('new', 'active', 'closed');
ALTER TYPE status ADD VALUE 'archived' AFTER 'closed';
CREATE TYPE address AS (street text, city text, zip varchar(10));
CREATE DOMAIN email AS citext CHECK (VALUE ~ '^[^@]+@[^@]+$');
CREATE DOMAIN positive_int AS integer NOT NULL DEFAULT 1 CONSTRAINT positive CHECK (VALUE > 0);
CREATE TYPE floatrange AS RANGE (subtype = float8, subtype_diff = float8mi);
CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE SEQUENCE invoice_no START 1000 INCREMENT 7 CACHE 1;
CREATE TABLE customer (
  id bigint GENERATED ALWAYS AS IDENTITY (START WITH 10) PRIMARY KEY,
  email email NOT NULL UNIQUE,
  name text COLLATE ci NOT NULL,
  status status NOT NULL DEFAULT 'new',
  home address,
  tags text[] NOT NULL DEFAULT '{}',
  scores integer[],
  prefs jsonb NOT NULL DEFAULT '{}',
  attrs hstore,
  credit numeric(12, 2) CHECK (credit >= 0),
  window_ tstzrange,
  span floatrange,
  name_len integer GENERATED ALWAYS AS (length(name)) STORED,
  created timestamptz NOT NULL DEFAULT '2024-01-01 00:00:00+00',
  photo bytea
);
ALTER TABLE customer OWNER TO a263_owner;
COMMENT ON TABLE customer IS 'who buys';
COMMENT ON COLUMN customer.prefs IS 'settings blob';
CREATE INDEX customer_lower_email ON customer (lower(email::text));
CREATE INDEX customer_active ON customer (created DESC) WHERE status = 'active';
CREATE INDEX customer_prefs ON customer USING gin (prefs jsonb_path_ops);
CREATE INDEX customer_name_trgm ON customer USING gist (name gist_trgm_ops);
CREATE INDEX customer_tags ON customer USING gin (tags);
CREATE INDEX customer_window ON customer USING gist (window_);
CREATE INDEX customer_cover ON customer (status) INCLUDE (name);
CREATE STATISTICS customer_stats (dependencies, ndistinct) ON status, name_len FROM customer;
CREATE TABLE product (
  sku text PRIMARY KEY,
  title text NOT NULL,
  price numeric(10, 2) NOT NULL,
  qty positive_int,
  weight real,
  ratio double precision,
  dims integer[][]
);
CREATE TABLE orders (
  id bigserial,
  customer_id bigint NOT NULL REFERENCES customer (id) ON DELETE CASCADE,
  invoice integer NOT NULL DEFAULT nextval('invoice_no'),
  placed date NOT NULL,
  total numeric(12, 2) NOT NULL DEFAULT 0,
  note text,
  PRIMARY KEY (id, placed)
) PARTITION BY RANGE (placed);
CREATE TABLE orders_2023 PARTITION OF orders FOR VALUES FROM ('2023-01-01') TO ('2024-01-01');
CREATE TABLE orders_2024 PARTITION OF orders FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')
  PARTITION BY RANGE (placed);
CREATE TABLE orders_2024_h1 PARTITION OF orders_2024 FOR VALUES FROM ('2024-01-01') TO ('2024-07-01');
CREATE TABLE orders_2024_h2 PARTITION OF orders_2024 FOR VALUES FROM ('2024-07-01') TO ('2025-01-01');
CREATE TABLE orders_rest PARTITION OF orders DEFAULT;
CREATE TABLE metric (at date NOT NULL, v integer) PARTITION BY LIST ((extract(month FROM at)::int));
CREATE TABLE metric_q1 PARTITION OF metric FOR VALUES IN (1, 2, 3);
CREATE TABLE metric_rest PARTITION OF metric DEFAULT;
CREATE INDEX metric_v ON metric (v) WHERE v > 0;
CREATE INDEX orders_customer ON orders (customer_id);
ALTER SEQUENCE invoice_no OWNED BY orders.invoice;
CREATE TABLE order_line (
  order_id bigint NOT NULL,
  placed date NOT NULL,
  line smallint NOT NULL,
  sku text NOT NULL REFERENCES product (sku) DEFERRABLE INITIALLY DEFERRED,
  qty integer NOT NULL CHECK (qty > 0),
  PRIMARY KEY (order_id, placed, line),
  FOREIGN KEY (order_id, placed) REFERENCES orders (id, placed) ON DELETE CASCADE
);
CREATE TABLE booking (
  id serial PRIMARY KEY,
  room integer NOT NULL,
  during tsrange NOT NULL,
  EXCLUDE USING gist (room WITH =, during WITH &&)
);
CREATE TABLE event_log (id bigserial PRIMARY KEY, at timestamptz NOT NULL DEFAULT now(), kind text);
CREATE TABLE event_click (x integer, y integer) INHERITS (event_log);
CREATE UNLOGGED TABLE cache_entry (k text PRIMARY KEY, v jsonb);
CREATE TABLE audit.change (
  id bigserial PRIMARY KEY,
  tbl regclass NOT NULL,
  op char(1) NOT NULL,
  row_data jsonb,
  at timestamptz NOT NULL DEFAULT '2024-01-01'
);
CREATE FUNCTION audit.record() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
  SET search_path = audit, pg_catalog AS $$
BEGIN
  INSERT INTO audit.change (tbl, op, row_data) VALUES (TG_RELID, left(TG_OP, 1), to_jsonb(coalesce(NEW, OLD)));
  RETURN coalesce(NEW, OLD);
END $$;
CREATE TRIGGER product_audit AFTER INSERT OR UPDATE OR DELETE ON product
  FOR EACH ROW EXECUTE FUNCTION audit.record();
CREATE FUNCTION order_total(o bigint) RETURNS numeric LANGUAGE sql STABLE
  RETURN (SELECT coalesce(sum(l.qty * p.price), 0) FROM order_line l JOIN product p USING (sku) WHERE l.order_id = o);
CREATE FUNCTION add(a integer, b integer DEFAULT 1) RETURNS integer LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
  AS 'SELECT a + b';
CREATE PROCEDURE close_customer(cid bigint) LANGUAGE plpgsql AS $$
BEGIN UPDATE customer SET status = 'closed' WHERE id = cid; END $$;
CREATE AGGREGATE product_of(numeric) (sfunc = numeric_mul, stype = numeric, initcond = '1');
CREATE OPERATOR === (leftarg = text, rightarg = text, function = texteq, commutator = ===);
CREATE VIEW active_customer WITH (security_barrier) AS
  SELECT id, email, name FROM customer WHERE status = 'active' WITH CASCADED CHECK OPTION;
CREATE VIEW order_summary AS
  SELECT o.id, o.placed, c.name, order_total(o.id) AS total
  FROM orders o JOIN customer c ON c.id = o.customer_id;
CREATE MATERIALIZED VIEW product_sales AS
  SELECT sku, sum(qty) AS sold FROM order_line GROUP BY sku WITH DATA;
CREATE UNIQUE INDEX product_sales_sku ON product_sales (sku);
CREATE RULE booking_no_delete AS ON DELETE TO booking WHERE old.room = 0 DO INSTEAD NOTHING;
ALTER TABLE customer ENABLE ROW LEVEL SECURITY;
CREATE POLICY customer_own ON customer FOR SELECT TO a263_reader USING (status <> 'closed');
GRANT USAGE ON SCHEMA app TO a263_reader;
GRANT SELECT ON customer, product TO a263_reader;
GRANT SELECT (id, placed) ON orders TO a263_reader;
GRANT EXECUTE ON FUNCTION order_total(bigint) TO a263_reader;
REVOKE EXECUTE ON FUNCTION add(integer, integer) FROM PUBLIC;
ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO a263_reader;
CREATE TEXT SEARCH CONFIGURATION app.simple_en (COPY = pg_catalog.english);
CREATE TABLE doc (id integer PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('app.simple_en', body)) STORED);
CREATE INDEX doc_tsv ON doc USING gin (tsv);
-- Data.
INSERT INTO customer (email, name, status, home, tags, scores, prefs, attrs, credit, window_, span, photo)
SELECT 'u' || g || '@ex.com', 'Name ' || g,
  (ARRAY['new', 'active', 'closed', 'archived'])[1 + g % 4]::status,
  ROW('street ' || g, 'city ' || (g % 7), lpad(g::text, 5, '0'))::address,
  ARRAY['t' || (g % 3), 't' || (g % 5)], ARRAY[g, g * 2, NULL],
  jsonb_build_object('n', g, 'even', g % 2 = 0, 'list', jsonb_build_array(g, 'x')),
  hstore('k' || (g % 4), g::text), (g * 3.25)::numeric(12, 2),
  tstzrange('2024-01-01'::timestamptz + g * interval '1 day', '2024-01-01'::timestamptz + (g + 3) * interval '1 day'),
  floatrange(g, g + 0.5), decode(md5(g::text), 'hex')
FROM generate_series(1, 300) g;
INSERT INTO product SELECT 'sku' || g, 'Product "' || g || E'"\ttab', g * 1.5, 1 + g % 9, g / 3.0, g / 7.0,
  ARRAY[[g, g + 1], [g + 2, g + 3]] FROM generate_series(1, 50) g;
INSERT INTO orders (customer_id, placed, total, note)
SELECT 10 + g % 300, DATE '2023-06-01' + (g * 5 % 700), g * 2.5, CASE WHEN g % 10 = 0 THEN NULL ELSE E'note\n' || g END
FROM generate_series(1, 400) g;
INSERT INTO order_line SELECT o.id, o.placed, l, 'sku' || (1 + (o.id * l) % 50), l FROM orders o, generate_series(1, 3) l;
INSERT INTO booking (room, during) SELECT g % 10, tsrange('2024-01-01'::timestamp + g * interval '1 hour', '2024-01-01'::timestamp + g * interval '1 hour' + interval '30 min') FROM generate_series(1, 100) g;
INSERT INTO event_log (at, kind) SELECT '2024-01-01'::timestamptz + g * interval '1 min', 'k' || g % 3 FROM generate_series(1, 50) g;
INSERT INTO event_click (at, kind, x, y) SELECT '2024-02-01'::timestamptz + g * interval '1 min', 'click', g, -g FROM generate_series(1, 30) g;
INSERT INTO metric SELECT DATE '2024-01-01' + g * 3, g - 20 FROM generate_series(1, 60) g;
INSERT INTO cache_entry SELECT 'k' || g, jsonb_build_object('v', g) FROM generate_series(1, 20) g;
INSERT INTO doc SELECT g, 'the quick brown fox ' || g || ' jumps over lazy dogs' FROM generate_series(1, 40) g;
UPDATE audit.change SET at = '2024-01-01';
REFRESH MATERIALIZED VIEW product_sales;
SELECT lo_from_bytea(424242, 'large object payload'::bytea);
COMMENT ON LARGE OBJECT 424242 IS 'a blob';
ANALYZE;
