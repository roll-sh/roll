-- Hasura's metadata database (hasura_meta: its hdb_catalog schema) and the source it serves
-- (hasura_ds): authors and articles (a foreign key), tags through a junction table,
-- comments, an enum table, a view, a set-returning function (tracked as a query root
-- field), a computed field's function, check and unique constraints the mutations hit,
-- jsonb and array columns.
DROP DATABASE IF EXISTS hasura_meta;
DROP DATABASE IF EXISTS hasura_ds;
CREATE DATABASE hasura_meta;
CREATE DATABASE hasura_ds;
\c hasura_ds
CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE TABLE article_status (value text PRIMARY KEY, comment text);
INSERT INTO article_status VALUES ('draft', 'Not published'), ('published', 'Visible'), ('archived', 'Hidden');
CREATE TABLE authors (
  id serial PRIMARY KEY,
  name text NOT NULL UNIQUE,
  email text CHECK (email LIKE '%@%'),
  bio jsonb NOT NULL DEFAULT '{}',
  rating numeric(3, 1),
  created_at timestamptz NOT NULL DEFAULT '2026-01-01 00:00+00'
);
CREATE TABLE articles (
  id serial PRIMARY KEY,
  author_id int NOT NULL REFERENCES authors ON DELETE CASCADE,
  title text NOT NULL,
  body text NOT NULL DEFAULT '',
  status text NOT NULL DEFAULT 'draft' REFERENCES article_status,
  views bigint NOT NULL DEFAULT 0,
  labels text[] NOT NULL DEFAULT '{}',
  published_on date,
  uuid uuid NOT NULL DEFAULT gen_random_uuid()
);
CREATE TABLE tags (id serial PRIMARY KEY, name text NOT NULL UNIQUE);
CREATE TABLE article_tags (
  article_id int REFERENCES articles ON DELETE CASCADE,
  tag_id int REFERENCES tags ON DELETE CASCADE,
  PRIMARY KEY (article_id, tag_id)
);
CREATE TABLE comments (
  id serial PRIMARY KEY,
  article_id int NOT NULL REFERENCES articles ON DELETE CASCADE,
  who text NOT NULL,
  text text NOT NULL,
  score int CHECK (score BETWEEN -5 AND 5)
);
INSERT INTO authors (name, email, bio, rating) VALUES
  ('Ursula', 'ul@example.com', '{"country": "US", "awards": ["Hugo", "Nebula"]}', 4.8),
  ('Italo', 'ic@example.com', '{"country": "IT"}', 4.5),
  ('Wislawa', 'ws@example.com', '{"country": "PL", "awards": ["Nobel"]}', 4.9),
  ('Nobody', NULL, '{}', NULL);
INSERT INTO articles (author_id, title, body, status, views, labels, published_on)
SELECT 1 + i % 3, 'article ' || i, repeat('word ', i), (ARRAY['draft', 'published', 'archived'])[1 + i % 3],
       i * 37 % 1000, ARRAY['l' || (i % 4), 'x'], DATE '2025-01-01' + i * 9
FROM generate_series(1, 30) i;
INSERT INTO tags (name) VALUES ('sf'), ('poetry'), ('essay'), ('news');
INSERT INTO article_tags SELECT i, 1 + i % 4 FROM generate_series(1, 30) i;
INSERT INTO article_tags SELECT i, 1 + (i + 1) % 4 FROM generate_series(1, 30, 3) i;
INSERT INTO comments (article_id, who, text, score)
SELECT 1 + i % 30, 'reader ' || (i % 7), 'comment ' || i, i % 11 - 5 FROM generate_series(1, 90) i;
CREATE VIEW author_stats AS
SELECT a.id, a.name, count(r.id) AS articles, coalesce(sum(r.views), 0) AS views
FROM authors a LEFT JOIN articles r ON r.author_id = a.id GROUP BY a.id, a.name;
CREATE FUNCTION search_articles(q text) RETURNS SETOF articles LANGUAGE sql STABLE AS
  $$ SELECT * FROM articles WHERE title ILIKE '%' || q || '%' OR body ILIKE '%' || q || '%' $$;
CREATE FUNCTION article_words(a articles) RETURNS int LANGUAGE sql STABLE AS
  $$ SELECT coalesce(array_length(regexp_split_to_array(trim(a.body), '\s+'), 1), 0) $$;
