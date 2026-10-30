-- Metabase's own database (metabase_app: its Liquibase migrations) and the database it syncs
-- and queries (metabase_ds): a small shop with foreign keys between people, products,
-- orders and reviews, a view, an enum, a jsonb column (Metabase unfolds its keys), numeric,
-- date, time and timestamp types, comments (Metabase reads them as descriptions) and a
-- second schema.
DROP DATABASE IF EXISTS metabase_app;
DROP DATABASE IF EXISTS metabase_ds;
CREATE DATABASE metabase_app;
CREATE DATABASE metabase_ds;
\c metabase_ds
CREATE TYPE tier AS ENUM ('free', 'pro', 'team');
CREATE TABLE people (
  id serial PRIMARY KEY,
  name text NOT NULL,
  email text UNIQUE,
  city text,
  tier tier NOT NULL DEFAULT 'free',
  born date,
  joined timestamptz NOT NULL,
  prefs jsonb
);
COMMENT ON TABLE people IS 'Customers';
COMMENT ON COLUMN people.email IS 'Login e-mail';
CREATE TABLE products (
  id serial PRIMARY KEY,
  title text NOT NULL,
  category text NOT NULL,
  price numeric(10, 2) NOT NULL,
  rating real,
  created timestamp NOT NULL
);
CREATE TABLE orders (
  id bigserial PRIMARY KEY,
  person_id int NOT NULL REFERENCES people,
  product_id int NOT NULL REFERENCES products,
  quantity smallint NOT NULL,
  total numeric(12, 2) NOT NULL,
  discount double precision,
  placed timestamptz NOT NULL,
  ship_time time
);
CREATE INDEX orders_person ON orders (person_id);
CREATE TABLE reviews (
  id serial PRIMARY KEY,
  product_id int NOT NULL REFERENCES products ON DELETE CASCADE,
  reviewer text,
  stars int CHECK (stars BETWEEN 1 AND 5),
  body text,
  at timestamptz
);
INSERT INTO people (name, email, city, tier, born, joined, prefs)
SELECT 'person ' || i, 'p' || i || '@example.com', (ARRAY['Oslo', 'Lima', 'Pune', 'Kyiv'])[1 + i % 4],
       (ARRAY['free', 'pro', 'team'])[1 + i % 3]::tier, DATE '1970-01-01' + i * 211,
       TIMESTAMPTZ '2025-01-01 00:00+00' + make_interval(hours => i * 7),
       jsonb_build_object('theme', CASE WHEN i % 2 = 0 THEN 'dark' ELSE 'light' END, 'news', i % 3 = 0, 'size', i % 5)
FROM generate_series(1, 120) i;
INSERT INTO products (title, category, price, rating, created)
SELECT 'product ' || i, (ARRAY['Gizmo', 'Widget', 'Gadget', 'Doohickey'])[1 + i % 4], (i * 3.37)::numeric(10, 2),
       (i % 50) / 10.0, TIMESTAMP '2024-06-01' + make_interval(days => i)
FROM generate_series(1, 60) i;
INSERT INTO orders (person_id, product_id, quantity, total, discount, placed, ship_time)
SELECT 1 + i % 120, 1 + (i * 7) % 60, 1 + i % 4, ((1 + i % 4) * (1 + (i * 7) % 60) * 3.37)::numeric(12, 2),
       CASE WHEN i % 5 = 0 THEN (i % 7) * 1.5 END,
       TIMESTAMPTZ '2025-01-01 00:00+00' + make_interval(hours => i * 3), TIME '08:00' + make_interval(mins => i % 600)
FROM generate_series(1, 1500) i;
INSERT INTO reviews (product_id, reviewer, stars, body, at)
SELECT 1 + i % 60, 'reviewer ' || (i % 30), 1 + i % 5, repeat('good ', i % 4 + 1),
       TIMESTAMPTZ '2025-02-01 00:00+00' + make_interval(hours => i * 5)
FROM generate_series(1, 300) i;
CREATE VIEW category_sales AS
SELECT p.category, count(*) AS orders, sum(o.total) AS revenue
FROM orders o JOIN products p ON p.id = o.product_id GROUP BY p.category;
CREATE SCHEMA analytics;
CREATE TABLE analytics.daily (day date PRIMARY KEY, visits int, conversion numeric(5, 4));
INSERT INTO analytics.daily SELECT DATE '2025-01-01' + i, 100 + i * 3 % 50, (i % 10) / 100.0 FROM generate_series(0, 89) i;
ANALYZE;
